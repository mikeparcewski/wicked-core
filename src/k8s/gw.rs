//! The daemon's model gateway on the cluster (PARITY B8, B9, C4; core#1394): every operation the
//! Docker gateway ran as a helper container is an exec into the gateway pod's long-lived
//! `writer` sidecar, the only container that mounts the config and broker volumes read-write.
//! **0 container lifecycles per seat start.**
//!
//! The host-side state machine is [`crate::seat_gateway`]'s own and unchanged: the persisted
//! routes and units (`config/gateway.json`, `config/units.json`), the pending revocations
//! (`config/revoking.json`), the rendered config, the locks. Only the transport is new.
//!
//! **Secrets ride stdin, never argv.** An exec's command is a URL query on the API server (in its
//! audit log and the kubelet's), so a credential or a unit key is always on stdin, and every
//! write is `cat > tmp`, then a sha256 check against the digest on the argv, then `mv`: a client
//! killed mid-write can never commit a partial credential.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::render::{self, GatewaySpec, HostPort};
use super::{cluster, daemon_namespace_name, Called, Kube};
use crate::seat_gateway::{GatewayRecord, UnitKey};
use crate::worker_sandbox::FloorUnarmed;

const EXEC_TIMEOUT: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(120);
const POD_SPOOL_CAP_BYTES: u64 = 64 * 1024 * 1024;

/// The daemon's gateway as the engine drives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Gw {
    pub(crate) kube: Kube,
    /// The daemon namespace.
    pub(crate) ns: String,
    /// The gateway pod whose writer is exec'd into.
    pub(crate) pod: String,
    /// The `agw` Service's cluster IP (a seat's `hostAliases` entry).
    pub(crate) service_ip: String,
    /// The daemon's gateway directory (the host-side state).
    pub(crate) dir: PathBuf,
}

/// The pod identity and gateway restart count bounding a launch's logs (core#1546).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogState {
    pod: String,
    uid: String,
    restarts: u64,
    lease: Option<Arc<RecordLease>>,
}

impl LogState {
    pub(crate) fn from_pod(pod: &serde_json::Value) -> Option<Self> {
        let gateway = pod
            .pointer("/status/containerStatuses")?
            .as_array()?
            .iter()
            .find(|c| c["name"].as_str() == Some("gateway"))?;
        Some(Self {
            pod: pod.pointer("/metadata/name")?.as_str()?.to_string(),
            uid: pod.pointer("/metadata/uid")?.as_str()?.to_string(),
            restarts: gateway["restartCount"].as_u64()?,
            lease: None,
        })
    }
}

/// Whether current and previous logs can cover the launch (core#1546).
#[cfg(test)]
pub(crate) fn logs_complete(
    arm: Option<&LogState>,
    release: Option<&LogState>,
    previous_ok: bool,
    current_ok: bool,
) -> bool {
    let (Some(arm), Some(release)) = (arm, release) else {
        return false;
    };
    arm.pod == release.pod
        && arm.uid == release.uid
        && release
            .restarts
            .checked_sub(arm.restarts)
            .is_some_and(|delta| delta <= 1)
        && current_ok
        && (release.restarts == 0 || previous_ok)
}

/// A launch holds its coverage window open, including when another launch overlaps it.
#[derive(Debug)]
struct RecordLease {
    spool: Arc<RecordSpool>,
    id: String,
}

impl PartialEq for RecordLease {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for RecordLease {}

impl Drop for RecordLease {
    fn drop(&mut self) {
        // Serialize idle teardown with new leases. The worker never takes this lock.
        let mut worker = self.spool.worker.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.spool.state.lock().unwrap_or_else(|p| p.into_inner());
        state.leases.remove(&self.id);
        if state.leases.is_empty() {
            state.reset();
            drop(state);
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        } else {
            state.prune();
        }
    }
}

/// Kill the entire local kubectl group and reap its direct child before releasing resources.
/// A child already reaped is left alone: its pid (and so its group id) may have been reused.
/// An exited but unreaped child stays a zombie, so its pid cannot be reused before the kill.
fn stop_record_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(unix)]
    // SAFETY: each record child is spawned into its own process group.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        // Windows has no killpg; taskkill /T includes kubectl's descendants.
        use wicked_apps_core::HardenedCommand;
        let _ = std::process::Command::new("taskkill")
            .hardened()
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Debug)]
struct RecordFollower {
    child: Arc<Mutex<Child>>,
    writer: Option<std::thread::JoinHandle<()>>,
    failed: Arc<AtomicBool>,
}

impl Drop for RecordFollower {
    fn drop(&mut self) {
        stop_record_child(&mut self.child.lock().unwrap_or_else(|p| p.into_inner()));
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

#[derive(Debug)]
struct Coverage {
    uids: HashSet<String>,
    complete: bool,
}

#[derive(Debug)]
struct PodRecords {
    name: String,
    path: PathBuf,
    attached_after: u64,
    restarts: u64,
    follower: Option<RecordFollower>,
    sealed: bool,
    finished: bool,
    failed: bool,
    gone: bool,
    running: bool,
}

#[derive(Debug, Default)]
struct RecordState {
    pods: HashMap<String, PodRecords>,
    leases: HashMap<String, Coverage>,
    watch: Option<Child>,
    covered: bool,
}

impl RecordState {
    fn reset(&mut self) {
        self.covered = false;
        if let Some(mut watch) = self.watch.take() {
            stop_record_child(&mut watch);
        }
        for pod in self.pods.values_mut() {
            // Join writers before unlinking (including on Windows).
            pod.follower = None;
            Self::remove_file(&pod.path);
        }
        self.pods.clear();
    }

    fn remove_file(path: &Path) {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "wicked-core: could not remove gateway spool {}: {e}",
                    path.display()
                );
            }
        }
    }

    fn prune(&mut self) {
        self.pods.retain(|uid, pod| {
            if !pod.gone || self.leases.values().any(|l| l.uids.contains(uid)) {
                return true;
            }
            pod.follower = None;
            Self::remove_file(&pod.path);
            false
        });
    }

    fn gap(&mut self) {
        for lease in self.leases.values_mut() {
            lease.complete = false;
        }
    }

    fn interrupt(&mut self) {
        self.covered = false;
        self.gap();
    }

    fn mark_failed_pods(&mut self) {
        for lease in self.leases.values_mut() {
            if lease
                .uids
                .iter()
                .any(|uid| self.pods.get(uid).is_some_and(|pod| pod.failed))
            {
                lease.complete = false;
            }
        }
    }
}

/// Leases own the watch and bounded followers. Replacing a pod preserves earlier records
/// until the last covering lease closes; idle spools own no processes, threads or files.
#[derive(Debug)]
struct RecordSpool {
    gw: Gw,
    state: Mutex<RecordState>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    cap: u64,
}

impl RecordSpool {
    fn lease(gw: &Gw, uid: &str) -> Result<Arc<RecordLease>, String> {
        static SPOOLS: Mutex<Vec<Weak<RecordSpool>>> = Mutex::new(Vec::new());
        let mut spools = SPOOLS.lock().unwrap_or_else(|p| p.into_inner());
        spools.retain(|s| s.strong_count() > 0);
        let existing = spools
            .iter()
            .filter_map(Weak::upgrade)
            .find(|s| s.gw.dir == gw.dir);
        let spool = if let Some(spool) = existing {
            if spool.gw.kube != gw.kube {
                return Err("the gateway spool already belongs to another cluster setting".into());
            }
            spool
        } else {
            let records = gw.dir.join("records");
            std::fs::create_dir_all(&records).map_err(|e| e.to_string())?;
            // No lease from a previous daemon is live in this process.
            for entry in std::fs::read_dir(&records).map_err(|e| e.to_string())? {
                let path = entry.map_err(|e| e.to_string())?.path();
                if path.extension().is_some_and(|s| s == "log") {
                    std::fs::remove_file(path).map_err(|e| e.to_string())?;
                }
            }
            let spool = Arc::new(Self {
                gw: gw.clone(),
                state: Mutex::new(RecordState::default()),
                worker: Mutex::new(None),
                cap: POD_SPOOL_CAP_BYTES,
            });
            spools.push(Arc::downgrade(&spool));
            spool
        };
        drop(spools);
        spool.open_lease(uid)
    }

    fn open_lease(self: &Arc<Self>, uid: &str) -> Result<Arc<RecordLease>, String> {
        let mut worker = self.worker.lock().unwrap_or_else(|p| p.into_inner());
        let id = uuid::Uuid::new_v4().simple().to_string();
        let first = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let first = state.leases.is_empty();
            Self::reap(&mut state);
            for (uid, pod) in &mut state.pods {
                if pod.follower.is_none() && !pod.sealed && pod.running && !pod.gone {
                    let identity = LogState {
                        pod: pod.name.clone(),
                        uid: uid.clone(),
                        restarts: pod.restarts,
                        lease: None,
                    };
                    match self.follow(&identity, &pod.path, true) {
                        Ok((follower, offset)) => {
                            pod.follower = Some(follower);
                            pod.attached_after = offset;
                            pod.finished = false;
                            pod.failed = false;
                        }
                        Err(e) => {
                            eprintln!("wicked-core: could not resume gateway follower: {e}");
                            pod.failed = true;
                        }
                    }
                }
            }
            // The first lease is not exposed until startup succeeds. Discovery may
            // invalidate it before then; never overwrite such a gap at watch startup.
            let complete = state.covered || first;
            let mut uids: HashSet<_> = state
                .pods
                .iter()
                .filter(|(_, pod)| !pod.gone)
                .map(|(uid, _)| uid.clone())
                .collect();
            uids.insert(uid.to_string());
            state.leases.insert(id.clone(), Coverage { uids, complete });
            state.mark_failed_pods();
            first
        };
        if first {
            // Publish coverage only after both the list and the new watch are up.
            let start = self.snapshot().and_then(|version| {
                let stdout = self.start_watch(&version)?;
                let spool = Arc::clone(self);
                std::thread::Builder::new()
                    .name("gateway-records".into())
                    .spawn(move || spool.run_watch(version, stdout))
                    .map_err(|e| e.to_string())
            });
            match start {
                Ok(handle) => {
                    *worker = Some(handle);
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.mark_failed_pods();
                }
                Err(e) => {
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.leases.remove(&id);
                    state.reset();
                    return Err(e);
                }
            }
        }
        drop(worker);
        Ok(Arc::new(RecordLease {
            spool: Arc::clone(self),
            id,
        }))
    }

    fn run_watch(self: Arc<Self>, mut version: String, mut stdout: ChildStdout) {
        loop {
            let result = self.watch(&mut version, stdout);
            if self.idle() {
                return;
            }
            if let Err(e) = result {
                eprintln!("wicked-core: gateway record watch stopped: {e}");
                self.state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .interrupt();
                // A failed watch may need a fresh resourceVersion. snapshot's child is also
                // tracked, so the final lease can cancel it while it is reading the list.
                std::thread::sleep(Duration::from_millis(100));
                loop {
                    match self.snapshot() {
                        Ok(next) => {
                            version = next;
                            break;
                        }
                        Err(e) => {
                            if self.idle() {
                                return;
                            }
                            eprintln!("wicked-core: gateway record discovery failed: {e}");
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            }
            loop {
                match self.start_watch(&version) {
                    Ok(next) => {
                        stdout = next;
                        break;
                    }
                    Err(e) => {
                        {
                            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                            if state.leases.is_empty() {
                                return;
                            }
                            state.interrupt();
                        }
                        eprintln!("wicked-core: could not restart gateway watch: {e}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
    }

    fn idle(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .leases
            .is_empty()
    }

    // The state lock makes spawning atomic with the final lease's reset. Both list and watch
    // use this slot, so no kubectl can be spawned after reset or escape cancellation.
    fn start_stream(&self, args: &[&str], covered: bool) -> Result<ChildStdout, String> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.leases.is_empty() {
            return Err("gateway record spool is idle".into());
        }
        let mut cmd = self.gw.kube.cmd(Some(&self.gw.ns));
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().expect("piped gateway stdout");
        state.watch = Some(child);
        state.covered = covered;
        Ok(stdout)
    }

    fn finish_stream(&self) -> bool {
        // stdout can close before the process exits. Never block in wait while holding the
        // state lock: the final lease must still be able to kill a stalled child.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state.covered = false;
                let Some(child) = state.watch.as_mut() else {
                    return false;
                };
                let status = child.try_wait();
                if !matches!(status, Ok(None)) || Instant::now() >= deadline {
                    let ok = matches!(status, Ok(Some(status)) if status.success());
                    if let Some(mut child) = state.watch.take() {
                        stop_record_child(&mut child);
                    }
                    return ok;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn snapshot(&self) -> Result<String, String> {
        let selector =
            super::owner_selector(&self.gw.kube.owner, &[(super::ROLE_LABEL, "gateway")]);
        let stdout = self.start_stream(
            &[
                "get",
                "pods",
                "-l",
                &selector,
                "-o",
                "json",
                "--request-timeout=60s",
            ],
            false,
        )?;
        let list = serde_json::from_reader::<_, serde_json::Value>(stdout);
        let ok = self.finish_stream();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !ok || state.leases.is_empty() {
            return Err("gateway pod list stopped".into());
        }
        let list = list.map_err(|e| e.to_string())?;
        let version = list
            .pointer("/metadata/resourceVersion")
            .and_then(serde_json::Value::as_str)
            .ok_or("gateway pod list has no resourceVersion")?
            .to_string();
        let pods = list["items"]
            .as_array()
            .ok_or("gateway pod list has no items")?;
        for pod in state.pods.values_mut() {
            pod.gone = true;
            pod.running = false;
        }
        for pod in pods {
            self.observe(&mut state, pod, false);
        }
        Self::reap(&mut state);
        state.prune();
        Ok(version)
    }

    fn start_watch(&self, version: &str) -> Result<ChildStdout, String> {
        let selector =
            super::owner_selector(&self.gw.kube.owner, &[(super::ROLE_LABEL, "gateway")]);
        // `kubectl get` has no resource-version flag. Use the exact list checkpoint.
        let uri = format!(
            "/api/v1/namespaces/{}/pods?watch=true&allowWatchBookmarks=true\
             &resourceVersion={}&labelSelector={}",
            self.gw.ns,
            query_component(version),
            query_component(&selector),
        );
        self.start_stream(&["get", "--raw", &uri], true)
    }

    fn watch(&self, version: &mut String, stdout: ChildStdout) -> Result<(), String> {
        let stream = serde_json::Deserializer::from_reader(std::io::BufReader::new(stdout))
            .into_iter::<serde_json::Value>();
        let mut interrupted = false;
        for event in stream {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.leases.is_empty() {
                return Ok(());
            }
            match event {
                Ok(event) => match event["type"].as_str() {
                    Some("ADDED" | "MODIFIED" | "DELETED") => {
                        let Some(next) = event
                            .pointer("/object/metadata/resourceVersion")
                            .and_then(serde_json::Value::as_str)
                        else {
                            interrupted = true;
                            break;
                        };
                        self.observe(&mut state, &event["object"], event["type"] == "DELETED");
                        *version = next.to_string();
                        Self::reap(&mut state);
                        state.prune();
                    }
                    Some("BOOKMARK") => {
                        if let Some(next) = event
                            .pointer("/object/metadata/resourceVersion")
                            .and_then(serde_json::Value::as_str)
                        {
                            *version = next.to_string();
                        }
                    }
                    _ => {
                        interrupted = true;
                        break;
                    }
                },
                Err(_) => {
                    interrupted = true;
                    break;
                }
            }
        }
        if !interrupted {
            interrupted = !self.finish_stream();
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut child) = state.watch.take() {
            stop_record_child(&mut child);
        }
        if interrupted {
            state.interrupt();
            Err("pod event coverage was interrupted; active leases are incomplete".into())
        } else {
            // Normal API watch timeouts resume at the last event/bookmark, never a fresh list.
            Ok(())
        }
    }

    fn observe(&self, state: &mut RecordState, pod: &serde_json::Value, deleted: bool) {
        let Some(identity) = LogState::from_pod(pod) else {
            state.gap();
            return;
        };
        let statuses = pod
            .pointer("/status/containerStatuses")
            .and_then(serde_json::Value::as_array);
        let running = statuses.is_some_and(|cs| {
            cs.iter()
                .any(|c| c["name"] == "gateway" && c.pointer("/state/running").is_some())
        });
        let terminated = statuses.is_some_and(|cs| {
            cs.iter()
                .any(|c| c["name"] == "gateway" && c.pointer("/state/terminated").is_some())
        });
        if !deleted && !running && identity.restarts == 0 && !terminated {
            return;
        }
        if plain_name(&identity.uid).is_err() {
            state.gap();
            return;
        }
        for lease in state.leases.values_mut() {
            lease.uids.insert(identity.uid.clone());
        }
        let entry = state.pods.entry(identity.uid.clone()).or_insert_with(|| {
            let path = self
                .gw
                .dir
                .join("records")
                .join(format!("{}.log", identity.uid));
            let mut records = PodRecords {
                name: identity.pod.clone(),
                path,
                attached_after: 0,
                restarts: identity.restarts,
                follower: None,
                sealed: false,
                finished: false,
                failed: !running || deleted,
                gone: deleted,
                running,
            };
            if running && !deleted {
                match self.follow(&identity, &records.path, false) {
                    Ok((follower, offset)) => {
                        records.follower = Some(follower);
                        records.attached_after = offset;
                    }
                    Err(e) => {
                        eprintln!(
                            "wicked-core: could not follow gateway {}: {e}",
                            identity.pod
                        );
                        records.failed = true;
                    }
                }
            }
            records
        });
        entry.gone = deleted;
        entry.running = running && !deleted;
        // A single follower cannot cover a container restart under the same UID.
        let restarted = entry.restarts != identity.restarts;
        entry.failed |= restarted;
        entry.restarts = identity.restarts;
        if restarted {
            state.gap();
        }
    }

    fn reap(state: &mut RecordState) {
        for pod in state.pods.values_mut() {
            if let Some(follower) = pod.follower.as_ref() {
                let status = follower
                    .child
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .try_wait();
                pod.sealed |= follower.failed.load(Ordering::Acquire);
                match status {
                    Ok(Some(status)) => {
                        // Join the writer before declaring EOF or inspecting its failure.
                        let follower = pod.follower.take().expect("active follower");
                        let failed = Arc::clone(&follower.failed);
                        drop(follower);
                        pod.sealed |= failed.load(Ordering::Acquire);
                        pod.finished = true;
                        pod.failed |= pod.sealed || !status.success() || pod.running;
                    }
                    Ok(None) => pod.failed |= pod.sealed,
                    Err(e) => {
                        eprintln!("wicked-core: gateway log follower failed: {e}");
                        pod.failed = true;
                    }
                }
            }
        }
        state.mark_failed_pods();
    }

    fn follow(
        &self,
        pod: &LogState,
        path: &Path,
        resume: bool,
    ) -> Result<(RecordFollower, u64), String> {
        use std::io::{Read as _, Write as _};
        let mut opts = std::fs::OpenOptions::new();
        opts.append(true).create_new(!resume);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = opts.open(path).map_err(|e| e.to_string())?;
        let offset = file.metadata().map_err(|e| e.to_string())?.len();
        if offset >= self.cap {
            return Err("gateway pod spool is at its size cap".into());
        }
        let mut cmd = self.gw.kube.cmd(Some(&self.gw.ns));
        cmd.args([
            "logs",
            "-f",
            // (core#1716) Each line carries the kubelet's stamp, so release can prove the follower
            // reached the gateway's latest line by its position, not its text.
            "--timestamps",
            "-c",
            "gateway",
            &pod.pod,
            "--pod-running-timeout=5s",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        if resume {
            // Earlier leases retain their gap. New launches append without replay.
            cmd.arg("--tail=0");
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let mut stdout = child.stdout.take().expect("piped gateway logs");
        let mut follower = RecordFollower {
            child: Arc::new(Mutex::new(child)),
            writer: None,
            failed: Arc::new(AtomicBool::new(false)),
        };
        let child = Arc::clone(&follower.child);
        let failed = Arc::clone(&follower.failed);
        let cap = self.cap;
        let writer = std::thread::Builder::new()
            .name("gateway-record-writer".into())
            .spawn(move || {
                let result = (|| -> std::io::Result<()> {
                    let mut written = offset;
                    let mut buf = [0; 8192];
                    loop {
                        let count = match stdout.read(&mut buf) {
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            result => result?,
                        };
                        if count == 0 {
                            return Ok(());
                        }
                        let allowed = (cap - written).min(count as u64) as usize;
                        if allowed < count {
                            failed.store(true, Ordering::Release);
                        }
                        file.write_all(&buf[..allowed])?;
                        written += allowed as u64;
                        if allowed < count {
                            return Err(std::io::Error::other(
                                "gateway pod spool size cap reached",
                            ));
                        }
                    }
                })();
                if let Err(e) = result {
                    // Includes ENOSPC: no silent truncation or resumption after a disk error.
                    failed.store(true, Ordering::Release);
                    eprintln!("wicked-core: gateway record writer stopped: {e}");
                    stop_record_child(&mut child.lock().unwrap_or_else(|p| p.into_inner()));
                }
            })
            .map_err(|e| e.to_string())?;
        follower.writer = Some(writer);
        Ok((follower, offset))
    }

    /// (core#1716) The pods (by UID) of lease `id` whose live follower is PROVEN to have spooled the
    /// gateway's latest log line at release. A record still in flight through `kubectl logs -f`
    /// would otherwise be missing while the lease reads complete. First one bounded
    /// `kubectl logs --timestamps --tail=1` snapshot per live pod, then one shared bounded wait
    /// ([`CATCH_UP_WAIT`], started after the snapshots) for each snapshot line to reach its spool.
    /// The follower stamps its lines the same way, so the match is the line's own position in the
    /// stream (the kubelet's nanosecond stamp), never an earlier line with the same text. An empty
    /// snapshot proves nothing (a rotated log can leave the current file empty while the follower
    /// drains the old one). [`Self::records`] treats a
    /// live follower missing from the set (a failed snapshot, a wait that ran out, or a pod that
    /// joined the lease meanwhile) as incomplete. Never under the state lock.
    fn caught_up(&self, id: &str) -> HashSet<String> {
        let live: Vec<(String, String, PathBuf)> = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            Self::reap(&mut state);
            let Some(lease) = state.leases.get(id) else {
                return HashSet::new();
            };
            lease
                .uids
                .iter()
                .filter_map(|uid| state.pods.get(uid).map(|pod| (uid, pod)))
                .filter(|(_, pod)| pod.follower.is_some() && !pod.finished)
                .map(|(uid, pod)| (uid.clone(), pod.name.clone(), pod.path.clone()))
                .collect()
        };
        let snapshots: Vec<(String, String, PathBuf, Option<String>)> = live
            .into_iter()
            .filter_map(|(uid, pod, path)| match self.latest_line(&pod) {
                Ok(line) => Some((uid, pod, path, line)),
                Err(e) => {
                    eprintln!(
                        "wicked-core: could not read gateway {pod}'s latest log line ({e}); the \
                         launch's records are incomplete (core#1716)"
                    );
                    None
                }
            })
            .collect();
        let deadline = Instant::now() + CATCH_UP_WAIT;
        let mut proven = HashSet::new();
        for (uid, pod, path, line) in snapshots {
            // An empty current log proves nothing: a rotated log can leave it empty while the
            // follower still drains the old one (codex r2 on #1819). That follower stays unproven
            // until its stream ends (EOF) or the gateway logs again.
            let held = line.is_some_and(|line| spool_holds_line(&path, line.as_bytes(), deadline));
            if held {
                proven.insert(uid);
            } else {
                eprintln!(
                    "wicked-core: gateway {pod}'s follower had not reached its latest log line in \
                     {} s; the launch's records are incomplete (core#1716)",
                    CATCH_UP_WAIT.as_secs()
                );
            }
        }
        proven
    }

    /// The gateway container's latest log line as the follower spools it (kubelet-stamped), `None`
    /// when its current log is empty. The runtime splits a long line into several log entries,
    /// which `--tail` counts one by one while a reader joins them under the FIRST entry's stamp
    /// (codex r2 on #1819), so the snapshot takes the last [`CATCH_UP_TAIL_ENTRIES`] entries and
    /// keeps the last joined line: whole unless the line alone outruns the window, and then it
    /// matches nothing and the read stays incomplete.
    fn latest_line(&self, pod: &str) -> Result<Option<String>, String> {
        let out = self.gw.kube.call_capped(
            Some(&self.gw.ns),
            &[
                "logs",
                "--timestamps",
                "-c",
                "gateway",
                pod,
                &format!("--tail={CATCH_UP_TAIL_ENTRIES}"),
                "--request-timeout=5s",
            ],
            None,
            CATCH_UP_SNAPSHOT_LIMIT,
            CATCH_UP_SNAPSHOT_CAP,
        );
        if !out.ok() {
            return Err(out.err_text());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .rev()
            .find(|l| !l.is_empty())
            .map(str::to_string))
    }

    fn records(&self, id: &str, unit: &str) -> (Vec<GatewayRecord>, bool) {
        let proven = self.caught_up(id);
        self.read_records(id, unit, &proven)
    }

    /// [`Self::records`] given the pods proven caught up ([`Self::caught_up`]).
    fn read_records(
        &self,
        id: &str,
        unit: &str,
        proven: &HashSet<String>,
    ) -> (Vec<GatewayRecord>, bool) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        Self::reap(&mut state);
        let watching = state
            .watch
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        let Some(lease) = state.leases.get(id) else {
            return (Vec::new(), false);
        };
        let mut complete = lease.complete && state.covered && watching;
        let mut records = Vec::new();
        for uid in &lease.uids {
            let Some(pod) = state.pods.get(uid) else {
                complete = false;
                continue;
            };
            complete &= !pod.failed && (!pod.gone || pod.finished);
            // (core#1716) A live follower must have been proven caught up; one that joined the
            // lease while the proof ran never was.
            complete &= pod.follower.is_none() || pod.finished || proven.contains(uid);
            match std::fs::read_to_string(&pod.path) {
                Ok(log) => {
                    // An empty, still-pending stream has not demonstrated attachment yet.
                    complete &= (log.len() as u64 > pod.attached_after || pod.finished)
                        && (log.is_empty() || log.ends_with('\n'));
                    records.extend(crate::seat_gateway::parse_records(
                        &strip_log_stamps(&log),
                        unit,
                    ));
                    complete &= !pod
                        .follower
                        .as_ref()
                        .is_some_and(|f| f.failed.load(Ordering::Acquire));
                }
                Err(_) => complete = false,
            }
        }
        (records, complete)
    }
}

/// (core#1716) How long a release waits for a live follower to spool the gateway's latest line.
const CATCH_UP_WAIT: Duration = Duration::from_secs(3);
/// (core#1716) The bound on the `kubectl logs --tail=1` snapshot, and on what it may return.
const CATCH_UP_SNAPSHOT_LIMIT: Duration = Duration::from_secs(10);
const CATCH_UP_SNAPSHOT_CAP: u64 = 8 * 1024 * 1024;
/// (core#1716) How many runtime log entries the snapshot reads: a line split into more entries
/// than this (each up to the runtime's line size, 16 KiB on containerd) reads as not caught up.
const CATCH_UP_TAIL_ENTRIES: u32 = 64;

/// (core#1716) Whether the spool at `path` holds `line` as one whole line, polling the appended
/// bytes until `deadline` (one pass at least). Reads only what was appended since the last poll
/// (the spool may hold up to [`POD_SPOOL_CAP_BYTES`]), and compares complete lines only.
fn spool_holds_line(path: &Path, line: &[u8], deadline: Instant) -> bool {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut from = 0u64;
    let mut partial: Vec<u8> = Vec::new();
    loop {
        if let Ok(mut file) = std::fs::File::open(path) {
            let mut appended = Vec::new();
            if file.seek(SeekFrom::Start(from)).is_ok() && file.read_to_end(&mut appended).is_ok() {
                from += appended.len() as u64;
                partial.extend_from_slice(&appended);
                if let Some(end) = partial.iter().rposition(|b| *b == b'\n') {
                    if partial[..end].split(|b| *b == b'\n').any(|l| l == line) {
                        return true;
                    }
                    partial.drain(..=end);
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// (core#1716) The spool's lines without the kubelet stamp the follower adds
/// (`<RFC3339 stamp> <line>`): the access-log records start at `{`. A line with no stamp is kept.
fn strip_log_stamps(log: &str) -> String {
    let mut out = String::with_capacity(log.len());
    for line in log.split_inclusive('\n') {
        let stamped = line
            .split_once(' ')
            .filter(|(stamp, _)| {
                stamp.len() >= 20
                    && stamp.as_bytes()[0].is_ascii_digit()
                    && stamp.ends_with('Z')
                    && stamp.contains('T')
            })
            .map(|(_, rest)| rest);
        out.push_str(stamped.unwrap_or(line));
    }
    out
}

/// Percent-encode an opaque resourceVersion or label selector as one query component.
fn query_component(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Where a writer write lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Volume {
    Config,
    Broker,
}

impl Volume {
    fn dir(self) -> &'static str {
        match self {
            Volume::Config => "/cfg",
            Volume::Broker => "/broker",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Volume::Config => "cfg",
            Volume::Broker => "broker",
        }
    }
}

/// The `$0` marker every write of `<v>/<file>` runs under (followed by a unique suffix): the
/// next write of that file finds an earlier one still running by it ([`write_script`]).
pub(crate) fn write_marker(v: Volume, file: &str) -> String {
    format!("wicked-writer-W-{}-{file}-", v.tag())
}

/// A plain file name a writer script may carry (no path, quote or shell character).
pub(crate) fn plain_name(file: &str) -> Result<(), String> {
    if file.is_empty()
        || file.starts_with('.')
        || !file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(format!("`{file}` is not a gateway file name"));
    }
    Ok(())
}

/// The writer script that replaces `<dir>/<file>` with stdin only when stdin's sha256 is
/// `digest` (pure: the argv carries the name and the digest, never the body).
pub(crate) fn write_script(v: Volume, file: &str, digest: &str) -> Result<String, String> {
    plain_name(file)?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("not a sha256 digest".into());
    }
    let d = v.dir();
    Ok(format!(
        "{} t={d}/.{file}.$$; trap 'rm -f \"$t\"' EXIT; cat > \"$t\"; \
         [ \"$(sha256sum \"$t\" | cut -d' ' -f1)\" = {digest} ] || {{ echo 'wicked-writer: the body did not arrive whole' >&2; exit 65; }}; \
         mv -f \"$t\" {d}/{file}; trap - EXIT",
        fence(v, file)
    ))
}

/// The writer script that removes `<dir>/<file>` behind the same fence as a write of it (B-4: a
/// journal or a staged record, which no route names).
pub(crate) fn remove_script(v: Volume, file: &str) -> Result<String, String> {
    plain_name(file)?;
    Ok(format!("{} rm -f {}/{file}", fence(v, file), v.dir()))
}

/// The fence every write or removal of `<v>/<file>` runs behind (see [`write_script`]), ending
/// with the file's kernel lock held on fd 9.
fn fence(v: Volume, file: &str) -> String {
    let d = v.dir();
    let m = write_marker(v, file);
    let first = &m[..1];
    let rest = &m[1..];
    format!(
        // The fence (review r3): an earlier write of this file whose client timed out may still
        // be running in the writer (exec does not end it), and could rename a stale body over
        // this one after it lands. Every such write is killed first (`$0` is its marker; `[x]y`
        // keeps the pattern from matching this script's own text), and any rename it already
        // started is waited out, before this body is taken. The kernel lock is the barrier (review
        // r5): every write holds it from before its body to after its rename (busybox `flock` has no
        // `-w`: a bounded `-n` retry, 20 s, found by the B-4 live run), and a rename child
        // an old write forked holds it too, so no stale rename can land after this one. A killed
        // write's body (a credential, codex b2-gw #7 / b4-r2 #3) is removed under the lock: no
        // earlier write is alive by then to rename it.
        "set -e; umask 077; \
         for p in $(pgrep -f '[{first}]{rest}'); do [ \"$p\" = \"$$\" ] || kill -9 \"$p\" 2>/dev/null || :; done; \
         i=0; while pgrep -f '[m]v -f {d}/.{file}.' >/dev/null; do i=$((i+1)); [ $i -lt 200 ] || exit 68; sleep 0.05; done; \
         exec 9>{d}/.{file}.lock; i=0; until flock -n 9; do i=$((i+1)); [ $i -lt 400 ] || {{ echo 'wicked-writer: an earlier write still holds the file' >&2; exit 69; }}; sleep 0.05; done; \
         rm -f {d}/.{file}.[0-9]*;"
    )
}

/// The readiness probe script: the key on stdin, then up to 15 s until the gateway answers it
/// with `want` (` 200 ` served, ` 401 ` refused) on its readiness listener.
pub(crate) fn confirm_script(accepted: bool) -> String {
    let want = if accepted { "HTTP/1.1 200" } else { " 401 " };
    format!(
        "read K; i=0; while [ $i -lt 60 ]; do \
         o=$(wget -T 2 -S -O /dev/null --header \"x-api-key: $K\" http://127.0.0.1:{}/ 2>&1); \
         case \"$o\" in *'{want}'*) exit 0;; esac; i=$((i+1)); sleep 0.25; done; exit 1",
        crate::seat_gateway::READY_PORT
    )
}

/// Kill the looping shell and confirm it is gone. Each wget has its own two-second timeout.
/// Bracket the marker so the cleanup exec cannot match itself.
fn cleanup_script(marker: &str) -> String {
    let pattern = format!("[{}]{}", &marker[..1], &marker[1..]);
    format!(
        "set -e; pkill -9 -f '{pattern}' || [ $? -eq 1 ]; \
         i=0; while :; do \
           if pgrep -f '{pattern}' >/dev/null; then \
             i=$((i+1)); [ $i -lt 50 ] || exit 70; sleep 0.1; \
           else code=$?; [ $code -eq 1 ] && exit 0; exit \"$code\"; fi; done"
    )
}

/// The broker-read script: for each named file that exists and is a regular file (no link on
/// the way), `name size` and the body (`presence` names are read as a marker only: `1` when
/// nonempty).
pub(crate) fn read_script(files: &[(&str, bool)]) -> Result<String, String> {
    let mut s = String::from("set -e;");
    for (f, presence) in files {
        plain_name(f)?;
        let p = format!("/broker/{f}");
        s.push_str(&format!(
            " if [ -f {p} ] && [ ! -L {p} ]; then n=$(wc -c < {p}); if [ $n -gt {max} ]; then exit 66; fi; "
            , max = crate::seat_login::MAX_FILE_BYTES
        ));
        if *presence {
            s.push_str(&format!(
                "if [ -s {p} ]; then printf '%s 1\\n1' {f}; else printf '%s 0\\n' {f}; fi; fi;"
            ));
        } else {
            s.push_str(&format!("printf '%s %s\\n' {f} $n; cat {p}; fi;"));
        }
    }
    Ok(s)
}

/// Parse [`read_script`]'s output (pure). `Err` on anything malformed.
pub(crate) fn parse_read(out: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut rest = out;
    while !rest.is_empty() {
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("a malformed broker read")?;
        let head = std::str::from_utf8(&rest[..nl]).map_err(|_| "a malformed broker read")?;
        let (name, size) = head.split_once(' ').ok_or("a malformed broker read")?;
        let size: usize = size.trim().parse().map_err(|_| "a malformed broker read")?;
        rest = &rest[nl + 1..];
        if rest.len() < size {
            return Err("a truncated broker read".into());
        }
        files.push((name.to_string(), rest[..size].to_vec()));
        rest = &rest[size..];
    }
    Ok(files)
}

impl Gw {
    /// One exec into the writer: `sh -c script` with `$0` = a unique marker (a dead engine's
    /// leftover is findable by it), stdin when given.
    pub(crate) fn exec(&self, script: &str, stdin: Option<&[u8]>, limit: Duration) -> Called {
        self.exec_marked("wicked-writer-", script, stdin, limit)
    }

    /// [`Gw::exec`] under `$0` = `prefix` + a unique suffix.
    fn exec_marked(
        &self,
        prefix: &str,
        script: &str,
        stdin: Option<&[u8]>,
        limit: Duration,
    ) -> Called {
        let marker = format!("{prefix}{}", uuid::Uuid::new_v4().simple());
        let mut args = vec!["exec"];
        if stdin.is_some() {
            args.push("-i");
        }
        args.extend([
            "-c",
            "writer",
            self.pod.as_str(),
            "--",
            "sh",
            "-c",
            script,
            &marker,
        ]);
        let mut result = self.kube.call(Some(&self.ns), &args, stdin, limit);
        if result.code.is_none() {
            let cleanup = cleanup_script(&marker);
            let cleaned = self.kube.call(
                Some(&self.ns),
                &[
                    "exec", "-c", "writer", &self.pod, "--", "sh", "-c", &cleanup,
                ],
                None,
                Duration::from_secs(10),
            );
            if !cleaned.ok() {
                result.stderr.extend_from_slice(
                    format!("\nremote exec cleanup failed: {}", cleaned.err_text()).as_bytes(),
                );
            }
        }
        result
    }

    /// Replace `<volume>/<file>` with `body`, whole or not at all.
    pub(crate) fn write_file(&self, v: Volume, file: &str, body: &[u8]) -> Result<(), String> {
        let digest = super::hex(&Sha256::digest(body));
        let script = write_script(v, file, &digest)?;
        // One write at a time from this engine: the fence in the script ends any earlier write
        // of the same file, which must only ever be an orphan, never a sibling of ours.
        let _one = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        let r = self.exec_marked(&write_marker(v, file), &script, Some(body), EXEC_TIMEOUT);
        if r.ok() {
            Ok(())
        } else {
            // The writer's stderr names no credential (the script prints none).
            Err(format!(
                "the gateway writer could not write {file}: {}",
                r.err_text()
            ))
        }
    }

    /// Remove `<volume>/<file>` behind the write fence (a journal or a staged record).
    pub(crate) fn remove_file(&self, v: Volume, file: &str) -> Result<(), String> {
        let script = remove_script(v, file)?;
        let _one = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        let r = self.exec_marked(&write_marker(v, file), &script, None, EXEC_TIMEOUT);
        if r.ok() {
            Ok(())
        } else {
            Err(format!(
                "the gateway writer could not remove {file}: {}",
                r.err_text()
            ))
        }
    }

    /// Whether no gateway can run or start: its Deployment is scaled to zero AND no gateway pod
    /// is left (codex r2: a pod still terminating could start the gateway on the previous
    /// config). `false` when either cannot be read.
    pub(crate) fn scaled_down(&self) -> bool {
        let no_pods = self
            .kube
            .list_owned(Some(&self.ns), "pods", &[(super::ROLE_LABEL, "gateway")])
            .is_ok_and(|p| p.is_empty());
        no_pods
            && self
                .kube
                .get_json(Some(&self.ns), "deployment", render::GATEWAY_NAME)
                .ok()
                .flatten()
                .and_then(|d| {
                    d.pointer("/spec/replicas")
                        .and_then(serde_json::Value::as_u64)
                })
                == Some(0)
    }

    /// Whether a gateway serves now, for a sign-out's confirmation ([`serving_state`]).
    pub(crate) fn serving_state(&self) -> Option<bool> {
        self.kube
            .list_owned(Some(&self.ns), "pods", &[(super::ROLE_LABEL, "gateway")])
            .ok()
            .and_then(|pods| serving_state(&pods))
    }

    /// One exec of `argv` in the gateway pod's `fetch` sidecar (the account reader), with
    /// `stdin`, its stdout capped at `cap`.
    pub(crate) fn fetch(&self, argv: &[String], stdin: &[u8], limit: Duration, cap: u64) -> Called {
        let mut args: Vec<&str> = vec!["exec", "-i", "-c", "fetch", self.pod.as_str(), "--"];
        args.extend(argv.iter().map(String::as_str));
        self.kube
            .call_capped(Some(&self.ns), &args, Some(stdin), limit, cap)
    }

    /// Empty a broker file (signed out): the gateway's routes reference it, so it is never
    /// deleted while a route names it.
    pub(crate) fn empty_broker_file(&self, file: &str) -> Result<(), String> {
        self.write_file(Volume::Broker, file, b"")
    }

    /// Read broker files (see [`read_script`]).
    pub(crate) fn read_broker(
        &self,
        files: &[(&str, bool)],
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        let r = self.exec(&read_script(files)?, None, EXEC_TIMEOUT);
        if !r.ok() {
            return Err(format!("could not read the broker: {}", r.err_text()));
        }
        parse_read(&r.stdout)
    }

    /// Publish the rendered `config/config.json` (the gateway's watcher reloads on the rename).
    pub(crate) fn publish(&self) -> Result<(), String> {
        let body = std::fs::read(self.dir.join("config/config.json"))
            .map_err(|e| format!("could not read the rendered gateway config: {e}"))?;
        self.write_file(Volume::Config, "config.json", &body)
    }

    /// Wait until the gateway serves (`accepted`) or refuses `key`; the key rides stdin.
    pub(crate) fn confirm_key(&self, key: &str, accepted: bool) -> Result<(), String> {
        let r = self.exec(
            &confirm_script(accepted),
            Some(format!("{key}\n").as_bytes()),
            EXEC_TIMEOUT,
        );
        if r.ok() {
            Ok(())
        } else {
            Err(format!(
                "the gateway did not {} the unit key in time \
                 (its config reload was not applied): {}",
                if accepted { "accept" } else { "revoke" },
                r.err_text()
            ))
        }
    }

    /// This gateway as it runs now (review r6 #15): the pod is looked up again by its labels (a
    /// quarantine or an eviction replaces it), the serving pod first, else the one whose writer
    /// runs; unchanged when none can be read.
    pub(crate) fn current(&self) -> Gw {
        self.find_current().unwrap_or_else(|| self.clone())
    }

    /// Observation needs a known current pod, including successful discovery (core#1546).
    fn find_current(&self) -> Option<Gw> {
        let pods = self
            .kube
            .list_owned(Some(&self.ns), "pods", &[(super::ROLE_LABEL, "gateway")])
            .ok()?;
        let pod = cluster::running_pod(&pods).or_else(|| cluster::writer_running(&pods))?;
        Some(Gw {
            pod,
            ..self.clone()
        })
    }

    /// The named pod's identity and gateway restart count, unknown on a failed read (core#1546).
    pub(crate) fn log_state(&self) -> Option<LogState> {
        let mut state = self
            .kube
            .get_json(Some(&self.ns), "pod", &self.pod)
            .ok()
            .flatten()
            .as_ref()
            .and_then(LogState::from_pod)?;
        match RecordSpool::lease(self, &state.uid) {
            Ok(lease) => state.lease = Some(lease),
            Err(e) => eprintln!("wicked-core: gateway records will be incomplete: {e}"),
        }
        Some(state)
    }

    /// The launch's access-log records and whether the available logs cover it (core#1546).
    pub(crate) fn records(
        &self,
        _since: &str,
        unit: &str,
        arm: Option<&LogState>,
    ) -> (Vec<GatewayRecord>, bool) {
        let Some(lease) = arm.and_then(|state| state.lease.as_ref()) else {
            return (Vec::new(), false);
        };
        lease.spool.records(&lease.id, unit)
    }
}

/// Whether a gateway serves (pure, review B-4 #5): `Some(true)` a pod serves (a generation can be
/// confirmed); `Some(false)` no gateway process is alive in any pod (none, or every gateway
/// container waiting or ended: nothing can still accept a pending key); `None` a gateway process
/// runs but is not ready (alive, so a pending revocation is never dropped on its account).
pub(crate) fn serving_state(pods: &[serde_json::Value]) -> Option<bool> {
    if cluster::running_pod(pods).is_some() {
        return Some(true);
    }
    let alive = pods.iter().any(|p| {
        p.pointer("/status/containerStatuses")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.get("name").and_then(serde_json::Value::as_str) == Some("gateway")
                        && c.pointer("/state/running").is_some()
                })
            })
    });
    if alive {
        None
    } else {
        Some(false)
    }
}

/// Serializes this engine's writer writes ([`Gw::write_file`]).
static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The empty credential file of every route (a reload naming a missing file is refused).
fn init_broker_script(config: &crate::seat_gateway::GatewayConfig) -> String {
    let mut s = String::from("set -e; umask 077;");
    for r in &config.providers {
        if let Some(f) = r.credential_file.file_name().and_then(|f| f.to_str()) {
            if plain_name(f).is_ok() {
                s.push_str(&format!(" [ -e /broker/{f} ] || : > /broker/{f};"));
            }
        }
    }
    s
}

/// The host's loopback as pods reach it: the first host alias the cluster's DNS resolves, from
/// the writer (`nslookup`), as an IP.
fn host_ip(gw: &Gw) -> Option<String> {
    crate::seat_gateway::HOST_ALIASES.iter().find_map(|alias| {
        let r = gw.exec(
            &format!("nslookup {alias} 2>/dev/null"),
            None,
            Duration::from_secs(15),
        );
        parse_nslookup(&String::from_utf8_lossy(&r.stdout))
    })
}

/// The answer address of busybox `nslookup` output (the last `Address:` line after `Name:`).
pub(crate) fn parse_nslookup(out: &str) -> Option<String> {
    let after = out.split_once("Name:")?.1;
    after
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("Address:")
                .or_else(|| l.trim().strip_prefix("Address 1:"))
        })
        .map(|a| a.split_whitespace().next().unwrap_or("").to_string())
        .find(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
}

/// The host ports the gateway must reach: the gate-hook relay and crew's loopback broker.
fn host_ports() -> Vec<u16> {
    let mut v = Vec::new();
    if let Some(p) = crate::hook_relay::port() {
        v.push(p);
    }
    if let Some(p) = crate::mcp_gate::daemon_crew_url()
        .as_deref()
        .and_then(crate::seat_gateway::crew_port)
    {
        v.push(p);
    }
    v
}

/// Probe the hook relay and the MCP broker from the gateway's own network namespace and record
/// the routes the next publish carries (once found, kept for the process).
fn probe_routes(gw: &Gw, ip: &str) {
    if let Some(port) = crate::hook_relay::port() {
        let up = format!("{ip}:{port}");
        crate::seat_gateway::probe_once(&crate::seat_gateway::HOOK_UPSTREAM, || {
            // A fresh egress rule applies within seconds (F1): retried briefly.
            (0..20).find_map(|_| {
                let r = gw.exec(
                    &format!("wget -q -T 2 -O - http://{up}/ping 2>/dev/null"),
                    None,
                    Duration::from_secs(10),
                );
                if r.ok() && String::from_utf8_lossy(&r.stdout).contains("pong") {
                    Some(up.clone())
                } else {
                    std::thread::sleep(Duration::from_millis(500));
                    None
                }
            })
        });
    }
    if let Some(port) = crate::mcp_gate::daemon_crew_url()
        .as_deref()
        .and_then(crate::seat_gateway::crew_port)
    {
        let up = format!("{ip}:{port}");
        crate::seat_gateway::probe_once(&crate::seat_gateway::MCP_UPSTREAM, || {
            (0..20).find_map(|_| {
                let r = gw.exec(
                    &format!("wget -S -T 2 -O /dev/null http://{up}/api/v1/health 2>&1"),
                    None,
                    Duration::from_secs(10),
                );
                if String::from_utf8_lossy(&r.stdout).contains("HTTP/1.") {
                    Some(up.clone())
                } else {
                    std::thread::sleep(Duration::from_millis(500));
                    None
                }
            })
        });
    }
}

/// The daemon's gateway, started or taken over, serving its current config (the cluster half
/// of gateway startup): keys an earlier engine left are confirmed refused before
/// any new unit runs.
pub(crate) fn ensure(kube: &Kube, dir: &Path) -> Result<Gw, (FloorUnarmed, String)> {
    let gwerr = |e: String| (FloorUnarmed::GatewayUnavailable, e);
    if !dir.is_absolute() {
        return Err(gwerr("the gateway directory must be absolute".into()));
    }
    let _broker = crate::seat_refresh::mutation_lock(dir).map_err(gwerr)?;
    // `config_transaction` holds the publish lock (review r1: taking it here too deadlocked).
    let _transaction = crate::seat_gateway::config_transaction(dir).map_err(gwerr)?;
    let ns = daemon_namespace_name(&kube.owner);
    let cluster_pending = cluster_rotation_pending(
        kube.get_json(Some(&ns), "deployment", render::GATEWAY_NAME)
            .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?
            .as_ref(),
    );
    let spec = GatewaySpec {
        namespace: ns.clone(),
        owner: kube.owner.clone(),
        image: crate::seat_gateway::GATEWAY_IMAGE.into(),
        writer_image: crate::seat_gateway::HELPER_IMAGE.into(),
        // A quarantine whose rotation did not land is finished first (review r2): the
        // gateway never starts again on the token it was stopped under.
        boot: boot_for_start(dir, cluster_pending).map_err(gwerr)?,
    };
    // Applied with the host ports already granted (idempotent; it also re-creates a gateway a
    // quarantine stopped, under its new boot token), then again once the host's address is
    // resolved from inside, when a port is missing.
    let existing = kube
        .get_json(Some(&ns), "networkpolicy", "gateway-egress")
        .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
    let mut host: Vec<HostPort> = existing.as_ref().map(render_host_ports).unwrap_or_default();
    let want = host_ports();
    cluster::apply_gateway(kube, &spec, &host).map_err(|e| (FloorUnarmed::PolicyRefused, e))?;
    let pod = cluster::writer_pod(kube, &ns, READY_TIMEOUT).map_err(gwerr)?;
    if cluster_pending {
        // The Deployment now runs on the rotated token: the owed rotation is paid.
        clear_cluster_rotation(&Gw {
            kube: kube.clone(),
            ns: ns.clone(),
            pod: pod.clone(),
            service_ip: String::new(),
            dir: dir.to_path_buf(),
        });
    }
    let mut gw = Gw {
        kube: kube.clone(),
        ns: ns.clone(),
        pod,
        service_ip: String::new(),
        dir: dir.to_path_buf(),
    };
    let ip = host_ip(&gw);
    if let Some(ip) = &ip {
        let missing: Vec<u16> = want
            .iter()
            .copied()
            .filter(|p| !host.iter().any(|h| &h.ip == ip && h.port == *p))
            .collect();
        if !missing.is_empty() {
            host.retain(|h| &h.ip == ip);
            host.extend(missing.into_iter().map(|port| HostPort {
                ip: ip.clone(),
                port,
            }));
            cluster::apply_gateway(kube, &spec, &host)
                .map_err(|e| (FloorUnarmed::PolicyRefused, e))?;
        }
        probe_routes(&gw, ip);
    }
    let mut config =
        crate::seat_gateway::provision_config(dir, crate::seat_gateway::egress_hosts_from_env())
            .map_err(gwerr)?;
    let r = gw.exec(&init_broker_script(&config), None, EXEC_TIMEOUT);
    if !r.ok() {
        return Err(gwerr(format!(
            "could not initialize the broker: {}",
            r.err_text()
        )));
    }
    let owned = crate::seat_gateway::OWNED
        .lock()
        .is_ok_and(|o| o.iter().any(|d| d == dir));
    if !owned && !config.units.is_empty() {
        // An earlier engine's keys: pending first, so a failure below still knows them.
        crate::seat_gateway::mark_revoking(dir, config.units.iter().map(|u| u.key.clone()))
            .map_err(gwerr)?;
        config.units.clear();
    }
    crate::seat_gateway::persist_units(dir, &config).map_err(gwerr)?;
    gw.publish().map_err(gwerr)?;
    // Only now may the gateway container start (a fresh pod waits for this token).
    gw.write_file(Volume::Config, "boot", spec.boot.as_bytes())
        .map_err(gwerr)?;
    let served = cluster::gateway_ready(kube, &ns, READY_TIMEOUT).map_err(gwerr)?;
    gw.pod = served.pod;
    gw.service_ip = served.service_ip;
    settle_revoking_locked(&gw).map_err(gwerr)?;
    if let Ok(mut o) = crate::seat_gateway::OWNED.lock() {
        if !o.iter().any(|d| d == dir) {
            o.push(dir.to_path_buf());
        }
    }
    Ok(gw)
}

/// The daemon's gateway for a sign-in (the `seat-login` CLI, B-4), never taking the daemon's
/// ownership: no config or boot token is published, no unit key is touched, and a quarantined
/// gateway is never restarted (its rotation is the daemon's to finish). `provision`: a first
/// sign-in creates the daemon's objects as [`ensure`] would (same boot token, the host ports the
/// egress policy already grants) when there is no gateway yet, so the broker's storage exists
/// before any run; the gateway container keeps waiting for the daemon's first config. An
/// existing gateway is never re-applied (a quarantine landing meanwhile stands). Without `provision`, `Ok(None)` when
/// the daemon has no gateway yet (nothing was ever signed in on this cluster).
pub(crate) fn attach(
    kube: &Kube,
    dir: &Path,
    provision: bool,
) -> Result<Option<Gw>, (FloorUnarmed, String)> {
    let gwerr = |e: String| (FloorUnarmed::GatewayUnavailable, e);
    let unreachable = |e: String| (FloorUnarmed::ClusterUnreachable, e);
    if !dir.is_absolute() {
        return Err(gwerr("the gateway directory must be absolute".into()));
    }
    let ns = daemon_namespace_name(&kube.owner);
    let quarantined = |dep: &Option<serde_json::Value>| {
        rotation_pending(dir)
            || cluster_rotation_pending(dep.as_ref())
            || dep
                .as_ref()
                .and_then(|d| d.pointer("/spec/replicas"))
                .and_then(serde_json::Value::as_u64)
                == Some(0)
    };
    let refused = || {
        gwerr(
            "the gateway is stopped after a revocation it could not confirm; the daemon \
             restarts it before its next seat; start a run, then try again"
                .into(),
        )
    };
    let dep = kube
        .get_json(Some(&ns), "deployment", render::GATEWAY_NAME)
        .map_err(unreachable)?;
    if quarantined(&dep) {
        return Err(refused());
    }
    if provision {
        let _broker = crate::seat_refresh::mutation_lock(dir).map_err(gwerr)?;
        let _transaction = crate::seat_gateway::config_transaction(dir).map_err(gwerr)?;
        let dep = kube
            .get_json(Some(&ns), "deployment", render::GATEWAY_NAME)
            .map_err(unreachable)?;
        if quarantined(&dep) {
            return Err(refused());
        }
        if let Some(d) = &dep {
            // Never re-applied here (review B-4 #2): only the daemon changes a gateway that
            // exists, so a quarantine that lands meanwhile is never undone by a sign-in.
            if !has_login_stores(d) {
                return Err(gwerr(
                    "the daemon's gateway predates sign-ins on the cluster; start a run so the \
                     daemon updates it, then sign in again"
                        .into(),
                ));
            }
        } else {
            let existing = kube
                .get_json(Some(&ns), "networkpolicy", "gateway-egress")
                .map_err(unreachable)?;
            let host: Vec<HostPort> = existing.as_ref().map(render_host_ports).unwrap_or_default();
            let spec = GatewaySpec {
                namespace: ns.clone(),
                owner: kube.owner.clone(),
                image: crate::seat_gateway::GATEWAY_IMAGE.into(),
                writer_image: crate::seat_gateway::HELPER_IMAGE.into(),
                boot: boot_token(dir, false).map_err(gwerr)?,
            };
            let (deployment, rest): (Vec<_>, Vec<_>) = cluster::daemon_docs(&spec, &host)
                .into_iter()
                .partition(|d| d["kind"] == "Deployment");
            kube.apply(&rest)
                .map_err(|e| (FloorUnarmed::PolicyRefused, e))?;
            // Created, never applied: one the daemon made (or quarantined) meanwhile stands.
            let body =
                serde_json::to_vec(&render::list(&deployment)).map_err(|e| gwerr(e.to_string()))?;
            let r = kube.call(
                None,
                &["create", "-f", "-"],
                Some(&body),
                super::CALL_TIMEOUT,
            );
            if !r.ok()
                && !r.err_text().contains("AlreadyExists")
                && !r.err_text().contains("already exists")
            {
                return Err((FloorUnarmed::PolicyRefused, r.err_text()));
            }
            let dep = kube
                .get_json(Some(&ns), "deployment", render::GATEWAY_NAME)
                .map_err(unreachable)?;
            if quarantined(&dep) {
                return Err(refused());
            }
        }
    } else if dep.is_none() {
        return Ok(None);
    }
    let pod = cluster::writer_pod(kube, &ns, READY_TIMEOUT).map_err(gwerr)?;
    Ok(Some(Gw {
        kube: kube.clone(),
        ns,
        pod,
        service_ip: String::new(),
        dir: dir.to_path_buf(),
    }))
}

/// Whether a gateway Deployment mounts the login stores (pure).
pub(crate) fn has_login_stores(dep: &serde_json::Value) -> bool {
    dep.pointer("/spec/template/spec/volumes")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|v| {
            v.iter().any(|x| {
                x.pointer("/persistentVolumeClaim/claimName")
                    .and_then(serde_json::Value::as_str)
                    == Some(render::GATEWAY_LOGINS_PVC)
            })
        })
}

/// The host ports an existing gateway egress policy names (pure).
pub(crate) fn render_host_ports(policy: &serde_json::Value) -> Vec<HostPort> {
    let mut out = Vec::new();
    for rule in policy
        .pointer("/spec/egress")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let cidr = rule
            .pointer("/to/0/ipBlock/cidr")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let Some(ip) = cidr.strip_suffix("/32") else {
            continue;
        };
        for p in rule
            .get("ports")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(port) = p.get("port").and_then(serde_json::Value::as_u64) {
                out.push(HostPort {
                    ip: ip.to_string(),
                    port: port as u16,
                });
            }
        }
    }
    out
}

fn settle_revoking_locked(gw: &Gw) -> Result<(), String> {
    let pending = crate::seat_gateway::rerender_without_pending(&gw.dir)?;
    if pending.is_empty() {
        return Ok(());
    }
    gw.publish()?;
    for key in &pending {
        gw.confirm_key(key, false)?;
    }
    crate::seat_gateway::atomic_json(
        &crate::seat_gateway::revoking_path(&gw.dir),
        &Vec::<String>::new(),
    )
}

/// Register `unit` and publish; the seat starts only once the gateway serves its key.
pub(crate) fn register_live(gw: &Gw, unit: UnitKey) -> Result<(), String> {
    let _broker = crate::seat_refresh::mutation_lock(&gw.dir)?;
    let _transaction = crate::seat_gateway::config_transaction(&gw.dir)?;
    let key = unit.key.clone();
    crate::seat_gateway::register_unit_locked(&gw.dir, unit)?;
    gw.publish()?;
    gw.confirm_key(&key, true)
}

/// Revoke `key`: complete only when the gateway refuses it (and every key still pending).
pub(crate) fn deregister_live(gw: &Gw, key: &str) -> Result<(), String> {
    let _broker = crate::seat_refresh::mutation_lock(&gw.dir)?;
    let _transaction = crate::seat_gateway::config_transaction(&gw.dir)?;
    crate::seat_gateway::deregister_unit_locked(&gw.dir, key)?;
    settle_revoking_locked(gw)
}

/// A revocation the gateway never confirmed: the gateway is stopped (scaled to none, its pods
/// waited out) and its boot token replaced, so no served policy outlives a key the engine meant
/// to revoke: the next [`ensure`] re-creates it, and its gateway container starts only after the
/// writer has published a config without the key and the new token (review r1). `true` when no
/// gateway pod is left.
pub(crate) fn quarantine(gw: &Gw) -> bool {
    let _publishing = crate::seat_gateway::PUBLISH_LOCK.lock();
    if let Ok(mut o) = crate::seat_gateway::OWNED.lock() {
        o.retain(|d| d != &gw.dir);
    }
    // Stopped first, whatever the host state allows (review r2, r4): the rotation owed is
    // recorded on the host when it can be (review r3) AND on the Deployment itself, in the same
    // patch that scales it to none, so an engine that dies (or a host that cannot write) still
    // leaves the next start owing it. A patch of the Deployment, never `scale`: the engine's
    // account holds no `deployments/scale` (review r4).
    let _ = begin_rotation(&gw.dir);
    let body = quarantine_patch().to_string();
    let r = gw.kube.call(
        Some(&gw.ns),
        &[
            "patch",
            "deployment",
            render::GATEWAY_NAME,
            "--type",
            "merge",
            "-p",
            &body,
        ],
        None,
        EXEC_TIMEOUT,
    );
    if !r.ok() {
        return false;
    }
    let start = Instant::now();
    while start.elapsed() < READY_TIMEOUT {
        match gw
            .kube
            .list_owned(Some(&gw.ns), "pods", &[(super::ROLE_LABEL, "gateway")])
        {
            Ok(p) if p.is_empty() => {
                if boot_token(&gw.dir, true).is_ok() {
                    let _ = end_rotation(&gw.dir);
                    clear_cluster_rotation(gw);
                }
                return true;
            }
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    false
}

/// The marker of a boot-token rotation a quarantine owes (`config/rotate-pending` of the gateway
/// dir): [`ensure`] rotates first, and refuses to start the gateway until it can. On disk, not in
/// the process (review r3): it outlives an engine that died mid-quarantine.
fn rotation_marker(dir: &Path) -> PathBuf {
    dir.join("config/rotate-pending")
}

/// The annotation a quarantine leaves on the gateway Deployment while its token rotation is owed.
pub(crate) const ROTATE_ANNOTATION: &str = "wicked.dev/rotate-pending";

/// The merge patch that stops the gateway and records the rotation it owes (pure).
pub(crate) fn quarantine_patch() -> serde_json::Value {
    serde_json::json!({
        "metadata": {"annotations": {ROTATE_ANNOTATION: "1"}},
        "spec": {"replicas": 0},
    })
}

/// Whether the gateway Deployment `dep` carries an owed rotation (pure).
pub(crate) fn cluster_rotation_pending(dep: Option<&serde_json::Value>) -> bool {
    dep.and_then(|d| d.pointer("/metadata/annotations"))
        .and_then(|a| a.get(ROTATE_ANNOTATION))
        .is_some()
}

/// Drop the Deployment's owed-rotation annotation (the new token is on disk). A failure leaves
/// it, and the next start rotates once more: never fewer rotations, only more.
fn clear_cluster_rotation(gw: &Gw) {
    let body = serde_json::json!({"metadata": {"annotations": {ROTATE_ANNOTATION: null}}});
    let _ = gw.kube.call(
        Some(&gw.ns),
        &[
            "patch",
            "deployment",
            render::GATEWAY_NAME,
            "--type",
            "merge",
            "-p",
            &body.to_string(),
        ],
        None,
        EXEC_TIMEOUT,
    );
}

/// The boot token a gateway (re)starts under: a fresh one when a quarantine still owes its
/// rotation (whichever engine began it; recorded on the host or, `cluster_pending`, on the
/// Deployment), else the current one.
pub(crate) fn boot_for_start(dir: &Path, cluster_pending: bool) -> Result<String, String> {
    let pending = rotation_pending(dir) || cluster_pending;
    let t = boot_token(dir, pending)?;
    if pending {
        end_rotation(dir)?;
    }
    Ok(t)
}

/// Whether a quarantine of `dir`'s gateway still owes its token rotation.
pub(crate) fn rotation_pending(dir: &Path) -> bool {
    std::fs::symlink_metadata(rotation_marker(dir)).is_ok()
}

/// Record that `dir`'s gateway owes a rotation (before it is stopped).
pub(crate) fn begin_rotation(dir: &Path) -> Result<(), String> {
    state_write(dir, &rotation_marker(dir), b"quarantined")
}

/// The gateway dir's `config` dir: a real directory (made when absent), never a link
/// (codex b2-gw #1).
fn state_dir(dir: &Path) -> Result<PathBuf, String> {
    let c = dir.join("config");
    std::fs::create_dir_all(&c).map_err(|e| e.to_string())?;
    match std::fs::symlink_metadata(&c) {
        Ok(m) if m.file_type().is_dir() => Ok(c),
        _ => Err(format!("{} is not a directory", c.display())),
    }
}

/// Replace one gateway state file: an owner-only file created new beside it, renamed over it
/// (a link planted at the name is replaced, never followed).
fn state_write(dir: &Path, path: &Path, body: &[u8]) -> Result<(), String> {
    let c = state_dir(dir)?;
    let tmp = c.join(format!(".state-{}", uuid::Uuid::new_v4().simple()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let wrote = o
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, body))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = wrote {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

/// A small gateway state file, only when it is a regular file (no link followed), bounded.
fn state_read(path: &Path) -> Option<String> {
    use std::io::Read as _;
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_file() && m.len() <= 256)?;
    let mut s = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(256)
        .read_to_string(&mut s)
        .ok()?;
    Some(s)
}

/// The rotation is done (the new token is on disk).
pub(crate) fn end_rotation(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(rotation_marker(dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

/// The gateway's boot token (`config/boot-token` of the gateway dir), created when absent or when
/// `fresh` (a quarantine).
pub(crate) fn boot_token(dir: &Path, fresh: bool) -> Result<String, String> {
    let path = dir.join("config/boot-token");
    if !fresh {
        if let Some(t) = state_read(&path) {
            let t = t.trim();
            // Only a token this engine wrote (a uuid's 32 hex digits).
            if t.len() == 32 && t.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(t.to_string());
            }
        }
    }
    let t = uuid::Uuid::new_v4().simple().to_string();
    state_write(dir, &path, t.as_bytes())?;
    Ok(t)
}

/// The sign-in a governed seat of `cli` runs on, read through the writer by the one sign-in
/// state machine (`seat_login::seat_sign_in`): an interrupted sign-in, sign-out or refresh is
/// settled first (C4). Signed out, expired or unreadable = the reason.
#[cfg(unix)]
pub(crate) fn seat_sign_in(
    gw: &Gw,
    cli: &str,
) -> Result<crate::seat_credentials::SeatSignIn, String> {
    crate::seat_login::seat_sign_in(&super::login::K8sIo::of(gw), &gw.dir, cli)
}

/// The claude setup-token a claude seat signed in with OAuth passes through (core#1024).
#[cfg(unix)]
pub(crate) fn claude_seat_token(gw: &Gw) -> Result<String, String> {
    crate::seat_login::claude_seat_token(&super::login::K8sIo::of(gw), &gw.dir)
}

/// Native Windows runs no seats (the seat runner needs a Unix host).
#[cfg(not(unix))]
pub(crate) fn seat_sign_in(
    _gw: &Gw,
    _cli: &str,
) -> Result<crate::seat_credentials::SeatSignIn, String> {
    Err("seat sign-ins need a Unix host".into())
}

/// Native Windows runs no seats (the seat runner needs a Unix host).
#[cfg(not(unix))]
pub(crate) fn claude_seat_token(_gw: &Gw) -> Result<String, String> {
    Err("seat sign-ins need a Unix host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct TestDir(PathBuf);

    #[cfg(unix)]
    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("wicked-gw-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    #[cfg(unix)]
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn wait_until(message: &str, mut ready: impl FnMut() -> bool) {
        let start = Instant::now();
        while !ready() {
            assert!(start.elapsed() < Duration::from_secs(5), "{message}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// (core#2028) A stub this file's tests exec is installed by `install_stub`, never written and
    /// chmod-ed in-process: a sibling test thread's fork can hold the write open into the first
    /// exec (`ETXTBSY`), which `log_state` reads as an unknown pod (the flaky `unwrap` on None).
    #[test]
    fn this_files_stubs_are_installed_out_of_process_2028() {
        let src = include_str!("gw.rs");
        let in_process = concat!("Permissions::from_", "mode(0o7");
        assert!(
            !src.contains(in_process),
            "a test stub in src/k8s/gw.rs is written in-process; use crate::test_env::install_stub"
        );
    }

    #[cfg(unix)]
    fn record_gateway(dir: &TestDir) -> Gw {
        let kubectl = dir.path().join("kubectl");
        // (core#2028) Through `install_stub`, as core#1769: this process never holds a writable
        // descriptor to the script it execs, so a first exec never fails with `ETXTBSY`.
        crate::test_env::install_stub(
            &kubectl,
            r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
# Each line the gateway logs gets the kubelet's stamp, unique and in order (core#1716).
stamp() {
    n=$(cat stamps 2>/dev/null || echo 0)
    while IFS= read -r line || [ -n "$line" ]; do
        n=$((n + 1))
        printf '2026-11-23T00:00:00.%09dZ %s\n' "$n" "$line"
    done
    echo "$n" > stamps
}
unstamp() { if [ "$stamped" = 1 ]; then cat; else cut -d ' ' -f 2-; fi; }
while [ "$1" != get ] && [ "$1" != logs ]; do shift; done
case "$1:$2" in
    get:pod) cat pod.json;;
    get:pods)
        if [ -f block-list ]; then
            sleep 300 &
            echo "$!" > list-descendant
            wait
            exit 23
        fi
        printf '{"metadata":{"resourceVersion":"1"},"items":['
        cat pod.json
        printf ']}';;
    get:--raw)
        sleep 300 &
        echo "$!" > watch-descendant
        while [ ! -f interrupt-watch ]; do sleep 0.02; done
        printf '{"type":"ERROR"}\n'
        wait;;
    logs:*)
        # The gateway's own log (core#1716), kubelet-stamped: `record`, then every `burst`,
        # whatever the follower has streamed. Without --timestamps a reader gets no stamps.
        case " $* " in *' --timestamps '*) stamped=1;; *) stamped=0;; esac
        [ -f gateway.log ] || stamp < record > gateway.log
        case " $* " in *' -f '*) ;; *)
            # The release snapshot: its tail, after an optional delay.
            if [ -f slow-snapshot ]; then sleep "$(cat slow-snapshot)"; fi
            tail -n 1 gateway.log | unstamp; exit 0;;
        esac
        echo "$$" >> followers
        sleep 300 &
        echo "$!" > follower-descendant
        # A fresh follower replays the gateway's log from its start: `record` as it is now.
        stamp < record > gateway.log
        sent=0
        while :; do
            if [ -f burst ]; then stamp < burst >> gateway.log; rm burst; fi
            # `hold`: the follower lags the gateway (a record in flight through logs -f).
            if [ ! -f hold ]; then
                size=$(wc -c < gateway.log)
                if [ "$size" -gt "$sent" ]; then tail -c +$((sent + 1)) gateway.log | unstamp; sent=$size; fi
            fi
            sleep 0.02
        done;;
esac
"#,
        );
        let pod = serde_json::json!({
            "metadata": {"name": "pod-1", "uid": "uid-1", "resourceVersion": "1"},
            "status": {"containerStatuses": [{"name": "gateway", "restartCount": 0,
                "ready": true, "state": {"running": {}}}]}
        });
        std::fs::write(dir.path().join("pod.json"), pod.to_string()).unwrap();
        std::fs::write(
            dir.path().join("record"),
            concat!(
                "{\"scope\":\"request\",\"wicked_unit\":\"launch@nonce\",",
                "\"wicked_run\":\"run\",\"http.status\":200,\"llm_model\":\"A\"}\n",
            ),
        )
        .unwrap();
        Gw {
            kube: Kube {
                kubectl,
                kubeconfig: None,
                context: "test".into(),
                owner: "test".into(),
            },
            ns: "test".into(),
            pod: "pod-1".into(),
            service_ip: String::new(),
            dir: dir.path().into(),
        }
    }

    #[cfg(unix)]
    fn process_running(pid: u32) -> bool {
        use wicked_apps_core::HardenedCommand;
        // ps also distinguishes orphaned zombies on systems whose init reaps slowly.
        let result = std::process::Command::new("ps")
            .hardened()
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&result.stdout);
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    #[cfg(unix)]
    #[test]
    fn last_record_lease_kills_children_removes_files_and_restarts_fresh() {
        let dir = TestDir::new();
        let gw = record_gateway(&dir);
        let arm = gw.log_state().unwrap();
        let overlap = gw.log_state().unwrap();
        let spool = Arc::clone(&arm.lease.as_ref().unwrap().spool);
        wait_until("initial records were not covered", || {
            gw.records("0", "launch@nonce", Some(&arm)).1
                && ["watch-descendant", "follower-descendant"]
                    .iter()
                    .all(|name| {
                        std::fs::read_to_string(dir.path().join(name))
                            .is_ok_and(|pid| pid.trim().parse::<u32>().is_ok())
                    })
        });
        let (watch, follower) = {
            let state = spool.state.lock().unwrap();
            let follower = state.pods["uid-1"]
                .follower
                .as_ref()
                .unwrap()
                .child
                .lock()
                .unwrap()
                .id();
            (state.watch.as_ref().unwrap().id(), follower)
        };
        let descendants: Vec<u32> = ["watch-descendant", "follower-descendant"]
            .iter()
            .map(|name| {
                std::fs::read_to_string(dir.path().join(name))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap()
            })
            .collect();
        drop(arm);
        assert!(process_running(watch));
        assert!(process_running(follower));
        assert!(dir.path().join("records/uid-1.log").is_file());
        drop(overlap);
        assert!(
            spool.worker.lock().unwrap().is_none(),
            "watch thread was not joined"
        );
        {
            let state = spool.state.lock().unwrap();
            assert!(state.watch.is_none());
            assert!(state.pods.is_empty());
            assert!(state.leases.is_empty());
            assert!(!state.covered);
        }
        assert_eq!(
            std::fs::read_dir(dir.path().join("records"))
                .unwrap()
                .count(),
            0
        );
        for pid in [watch, follower] {
            // The daemon must reap its direct children, not leave zombies behind.
            // SAFETY: signal zero only probes the PIDs of this test's kubectl children.
            assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        }
        for pid in descendants {
            wait_until("a record process group descendant survived", || {
                !process_running(pid)
            });
        }

        // Keep the idle spool alive to exercise its reset path, rather than a new registry entry.
        std::fs::write(dir.path().join("record"), b"fresh\n").unwrap();
        let next = gw.log_state().unwrap();
        assert!(Arc::ptr_eq(&spool, &next.lease.as_ref().unwrap().spool));
        wait_until("a later lease did not start fresh", || {
            std::fs::read_to_string(dir.path().join("records/uid-1.log"))
                .is_ok_and(|log| strip_log_stamps(&log) == "fresh\n")
        });
        assert!(spool.state.lock().unwrap().covered);
        assert!(gw.records("0", "launch@nonce", Some(&next)).0.is_empty());
        drop(next);
        assert!(spool.worker.lock().unwrap().is_none());
        assert!(!dir.path().join("records/uid-1.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn last_lease_cancels_discovery_during_watch_recovery() {
        let dir = TestDir::new();
        let gw = record_gateway(&dir);
        let arm = gw.log_state().unwrap();
        let spool = Arc::clone(&arm.lease.as_ref().unwrap().spool);
        wait_until("initial watch was not covered", || {
            gw.records("0", "launch@nonce", Some(&arm)).1
        });
        std::fs::write(dir.path().join("block-list"), b"").unwrap();
        std::fs::write(dir.path().join("interrupt-watch"), b"").unwrap();
        wait_until("recovery did not start discovery", || {
            std::fs::read_to_string(dir.path().join("list-descendant"))
                .is_ok_and(|pid| pid.trim().parse::<u32>().is_ok())
        });
        let list = spool.state.lock().unwrap().watch.as_ref().unwrap().id();
        let descendant: u32 = std::fs::read_to_string(dir.path().join("list-descendant"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!spool.state.lock().unwrap().covered);
        drop(arm);
        assert!(!process_running(list));
        wait_until("discovery's descendant survived", || {
            !process_running(descendant)
        });
        assert!(spool.worker.lock().unwrap().is_none());
        assert!(spool.state.lock().unwrap().pods.is_empty());
        assert_eq!(
            std::fs::read_dir(dir.path().join("records"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn capped_pod_stops_writing_and_invalidates_all_covering_leases() {
        let dir = TestDir::new();
        let gw = record_gateway(&dir);
        std::fs::create_dir_all(dir.path().join("records")).unwrap();
        let spool = Arc::new(RecordSpool {
            gw,
            state: Mutex::new(RecordState::default()),
            worker: Mutex::new(None),
            cap: 256,
        });
        let first = spool.open_lease("uid-1").unwrap();
        let second = spool.open_lease("uid-1").unwrap();
        wait_until("initial records were not covered", || {
            spool.records(&first.id, "launch@nonce").1
                && spool.records(&second.id, "launch@nonce").1
        });
        let pid = spool.state.lock().unwrap().pods["uid-1"]
            .follower
            .as_ref()
            .unwrap()
            .child
            .lock()
            .unwrap()
            .id();
        std::fs::write(dir.path().join("burst.tmp"), vec![b'x'; 2048]).unwrap();
        std::fs::rename(dir.path().join("burst.tmp"), dir.path().join("burst")).unwrap();
        wait_until("capped follower did not stop", || {
            let mut state = spool.state.lock().unwrap();
            RecordSpool::reap(&mut state);
            let pod = &state.pods["uid-1"];
            pod.sealed && pod.follower.is_none()
        });
        assert!(!process_running(pid));
        assert!(!spool.records(&first.id, "launch@nonce").1);
        assert!(!spool.records(&second.id, "launch@nonce").1);
        let path = dir.path().join("records/uid-1.log");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), spool.cap);

        let later = spool.open_lease("uid-1").unwrap();
        assert!(!spool.records(&later.id, "launch@nonce").1);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), spool.cap);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("followers"))
                .unwrap()
                .lines()
                .count(),
            1,
            "a capped pod must never resume within the active spool",
        );
        drop(first);
        drop(second);
        assert!(path.exists());
        drop(later);
        assert!(!path.exists());
        assert!(spool.worker.lock().unwrap().is_none());
    }

    /// core#1716 (red on main): at release the follower may still lag the gateway — a record the
    /// gateway logged is still in flight through `kubectl logs -f`. Release waits (bounded) for the
    /// follower to spool the gateway's latest line, and reports the records INCOMPLETE when it has
    /// not, never complete with the record missing. Red on main: `complete` was true with no record.
    #[cfg(unix)]
    #[test]
    fn a_lagging_follower_is_waited_for_or_reported_incomplete_1716() {
        let dir = TestDir::new();
        let gw = record_gateway(&dir);
        let arm = gw.log_state().unwrap();
        wait_until("initial records were not covered", || {
            gw.records("0", "launch@nonce", Some(&arm)).1
        });
        let late = |unit: &str| {
            format!(
                "{{\"scope\":\"request\",\"wicked_unit\":\"{unit}\",\"wicked_run\":\"run\",\
                 \"http.status\":200,\"llm_model\":\"B\"}}\n"
            )
        };
        let emit = |name: &str, body: &str| {
            let tmp = dir.path().join(format!("{name}.tmp"));
            std::fs::write(&tmp, body).unwrap();
            std::fs::rename(tmp, dir.path().join(name)).unwrap();
        };
        let gateway_holds = |unit: &str| {
            wait_until("the gateway did not log the record", || {
                std::fs::read_to_string(dir.path().join("gateway.log"))
                    .is_ok_and(|l| l.contains(unit))
            });
        };
        // The gateway logs the launch's record; the follower lags it.
        std::fs::write(dir.path().join("hold"), b"").unwrap();
        emit("burst", &late("launch@late"));
        gateway_holds("launch@late");
        let started = Instant::now();
        let (records, complete) = gw.records("0", "launch@late", Some(&arm));
        assert!(records.is_empty());
        assert!(!complete, "a record still in flight must not read complete");
        assert!(
            started.elapsed() >= CATCH_UP_WAIT,
            "release waited for the follower first"
        );
        // The follower catches up: the record is there, and the records are complete.
        std::fs::remove_file(dir.path().join("hold")).unwrap();
        wait_until("the follower did not catch up", || {
            gw.records("0", "launch@late", Some(&arm)).1
        });
        assert_eq!(gw.records("0", "launch@late", Some(&arm)).0.len(), 1);
        // A follower that catches up DURING the bounded wait completes the same read.
        std::fs::write(dir.path().join("hold"), b"").unwrap();
        emit("burst", &late("launch@soon"));
        gateway_holds("launch@soon");
        let release = {
            let hold = dir.path().join("hold");
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                std::fs::remove_file(hold).unwrap();
            })
        };
        let (records, complete) = gw.records("0", "launch@soon", Some(&arm));
        release.join().unwrap();
        assert_eq!(records.len(), 1);
        assert!(complete);
    }

    /// core#1716 (codex r1 on #1819): the follower's lines are kubelet-stamped, so a snapshot of
    /// the gateway's latest line matches its own position in the stream. For gateway output
    /// `A, B, A`, a spool holding only the first `A` is not caught up. The stamps are stripped
    /// before the records are parsed.
    #[test]
    fn a_stamped_snapshot_matches_its_own_position_and_records_parse_unstamped() {
        let a = r#"{"scope":"request","wicked_unit":"u","wicked_run":"r","http.status":200}"#;
        let b = r#"{"scope":"request","wicked_unit":"u","wicked_run":"r","http.status":500}"#;
        let first = format!("2026-11-23T00:00:01.000000001Z {a}");
        let last = format!("2026-11-23T00:00:03.000000003Z {a}");
        let dir = std::env::temp_dir().join(format!("wc-1716-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spool.log");
        std::fs::write(&path, format!("{first}\n")).unwrap();
        assert!(!spool_holds_line(&path, last.as_bytes(), Instant::now()));
        std::fs::write(
            &path,
            format!("{first}\n2026-11-23T00:00:02.000000002Z {b}\n{last}\n"),
        )
        .unwrap();
        assert!(spool_holds_line(&path, last.as_bytes(), Instant::now()));
        let log = std::fs::read_to_string(&path).unwrap();
        let parsed = crate::seat_gateway::parse_records(&strip_log_stamps(&log), "u");
        assert_eq!(parsed.len(), 3);
        assert_eq!(strip_log_stamps("{\"x\":1}\nplain\n"), "{\"x\":1}\nplain\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#1716 (codex r1 on #1819): the catch-up check assembles a line appended in fragments
    /// across polls, matches whole lines only, and gives up at a future deadline.
    #[cfg(unix)]
    #[test]
    fn spool_holds_line_waits_for_a_line_appended_in_fragments() {
        use std::io::Write as _;
        let dir = TestDir::new();
        let path = dir.path().join("spool.log");
        std::fs::write(&path, "alpha\nbeta-long\npar").unwrap();
        let now = Instant::now;
        assert!(spool_holds_line(&path, b"alpha", now()));
        assert!(
            !spool_holds_line(&path, b"beta", now()),
            "a prefix is not the line"
        );
        assert!(!spool_holds_line(
            &dir.path().join("none.log"),
            b"alpha",
            now()
        ));
        let started = now();
        assert!(!spool_holds_line(
            &path,
            b"partial",
            now() + Duration::from_millis(200)
        ));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(200) && waited < Duration::from_secs(2));
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                f.write_all(b"ti").unwrap();
                std::thread::sleep(Duration::from_millis(100));
                f.write_all(b"al\n").unwrap();
            })
        };
        assert!(spool_holds_line(
            &path,
            b"partial",
            now() + Duration::from_secs(2)
        ));
        writer.join().unwrap();
    }

    /// core#1716 (codex r2 on #1819): the proof matches the gateway's latest line by its stamped
    /// position, end to end through `records`. For gateway output `A, B, A` with the follower
    /// holding only the first `A`, the read is incomplete; once it catches up, all three records
    /// parse (their stamps stripped). Red with either `--timestamps` removed: the bare snapshot
    /// `A` matches the first `A` (complete, one record), or never matches a stamped spool.
    #[cfg(unix)]
    #[test]
    fn a_repeated_line_is_matched_by_its_stamped_position_through_records_1716() {
        let dir = TestDir::new();
        let gw = record_gateway(&dir);
        let arm = gw.log_state().unwrap();
        wait_until("initial records were not covered", || {
            gw.records("0", "launch@nonce", Some(&arm)).1
        });
        let line = |status: u16| {
            format!(
                "{{\"scope\":\"request\",\"wicked_unit\":\"launch@aba\",\"wicked_run\":\"run\",\
                 \"http.status\":{status}}}\n"
            )
        };
        let emit = |body: &str| {
            let tmp = dir.path().join("burst.tmp");
            std::fs::write(&tmp, body).unwrap();
            std::fs::rename(tmp, dir.path().join("burst")).unwrap();
            wait_until("the gateway did not log the burst", || {
                !dir.path().join("burst").exists()
            });
        };
        emit(&line(200));
        wait_until("the first A was not spooled", || {
            std::fs::read_to_string(dir.path().join("records/uid-1.log"))
                .is_ok_and(|l| l.contains("launch@aba"))
        });
        std::fs::write(dir.path().join("hold"), b"").unwrap();
        emit(&format!("{}{}", line(500), line(200)));
        let (records, complete) = gw.records("0", "launch@aba", Some(&arm));
        assert_eq!(records.len(), 1);
        assert!(!complete, "the first A is not the gateway's latest A");
        std::fs::remove_file(dir.path().join("hold")).unwrap();
        wait_until("the follower did not catch up", || {
            gw.records("0", "launch@aba", Some(&arm)).1
        });
        let (records, _) = gw.records("0", "launch@aba", Some(&arm));
        let statuses: Vec<u16> = records.iter().map(|r| r.status).collect();
        assert_eq!(statuses, [200, 500, 200]);
    }

    /// core#1716 (codex r2 on #1819): the follower's wait starts once the snapshot is in, so a slow
    /// snapshot does not eat it: a follower that catches up just after a 3.5 s snapshot is still
    /// proven. Red with the deadline set before the snapshot (it has run out by then).
    #[cfg(unix)]
    #[test]
    fn a_slow_snapshot_leaves_the_follower_its_whole_wait_1716() {
        let dir = TestDir::new();
        let gw = record_gateway(&dir);
        let arm = gw.log_state().unwrap();
        wait_until("initial records were not covered", || {
            gw.records("0", "launch@nonce", Some(&arm)).1
        });
        std::fs::write(dir.path().join("hold"), b"").unwrap();
        let body = "{\"scope\":\"request\",\"wicked_unit\":\"launch@slow\",\"wicked_run\":\"run\",\
                    \"http.status\":200}\n";
        std::fs::write(dir.path().join("burst.tmp"), body).unwrap();
        std::fs::rename(dir.path().join("burst.tmp"), dir.path().join("burst")).unwrap();
        wait_until("the gateway did not log the record", || {
            !dir.path().join("burst").exists()
        });
        std::fs::write(dir.path().join("slow-snapshot"), b"3.5").unwrap();
        let release = {
            let hold = dir.path().join("hold");
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(3800));
                std::fs::remove_file(hold).unwrap();
            })
        };
        let (records, complete) = gw.records("0", "launch@slow", Some(&arm));
        release.join().unwrap();
        assert_eq!(records.len(), 1);
        assert!(complete);
    }

    /// core#1716 (codex r1 on #1819): the proof is per pod. A live follower of the lease that is
    /// not in the proven set — a pod that joined while the proof ran — makes the read incomplete.
    #[cfg(unix)]
    #[test]
    fn a_live_follower_missing_from_the_proof_is_incomplete_1716() {
        let dir = TestDir::new();
        let gw = record_gateway(&dir);
        let arm = gw.log_state().unwrap();
        wait_until("initial records were not covered", || {
            gw.records("0", "launch@nonce", Some(&arm)).1
        });
        let lease = arm.lease.as_ref().unwrap();
        let proven: HashSet<String> = ["uid-1".to_string()].into();
        assert!(
            lease
                .spool
                .read_records(&lease.id, "launch@nonce", &proven)
                .1
        );
        assert!(
            !lease
                .spool
                .read_records(&lease.id, "launch@nonce", &HashSet::new())
                .1
        );
    }

    #[cfg(unix)]
    #[test]
    fn lease_records_survive_a_gateway_pod_replacement() {
        let dir = TestDir::new();
        let kubectl = dir.path().join("kubectl");
        // (core#2028) Through `install_stub`, as core#1769: this process never holds a writable
        // descriptor to the script it execs, so a first exec never fails with `ETXTBSY`.
        crate::test_env::install_stub(
            &kubectl,
            r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
stamp() {
    n=$(cat stamps 2>/dev/null || echo 0)
    while IFS= read -r line || [ -n "$line" ]; do
        n=$((n + 1))
        printf '2026-11-23T00:00:00.%09dZ %s\n' "$n" "$line"
    done
    echo "$n" > stamps
}
unstamp() { if [ "$stamped" = 1 ]; then cat; else cut -d ' ' -f 2-; fi; }
while [ "$1" != get ] && [ "$1" != logs ]; do shift; done
case "$1" in
get)
    if [ "$2" = pod ]; then cat pod.json
    else case " $* " in
        *' --raw '*)
            previous=$(cat watch-start.json)
            while [ -f pod.json ]; do
                current=$(cat pod.json)
                if [ "$current" != "$previous" ]; then
                    printf '{"type":"DELETED","object":%s}\n' "$previous"
                    printf '{"type":"ADDED","object":%s}\n' "$current"
                    previous=$current
                fi
                sleep 0.02
            done;;
        *) cp pod.json watch-start.json
           printf '{"metadata":{"resourceVersion":"1"},"items":['; cat pod.json; printf ']}';;
    esac; fi;;
logs)
    case " $* " in *' pod-1 '*) pod=1;; *) pod=2;; esac
    # Each pod's own log (core#1716), kubelet-stamped: its record, then every `more`; the
    # snapshot is its tail. Without --timestamps a reader gets no stamps.
    case " $* " in *' --timestamps '*) stamped=1;; *) stamped=0;; esac
    log="gateway-$pod.log"
    [ -f "$log" ] || stamp < "record-$pod" > "$log"
    case " $* " in *' -f '*) ;; *) tail -n 1 "$log" | unstamp; exit 0;; esac
    printf '%s\n' "$pod" >> followers
    case " $* " in *' --tail=0 '*) sent=$(wc -c < "$log");; *) sent=0;; esac
    while [ ! -f "end-$pod" ]; do
        if [ -f "more-$pod" ]; then stamp < "more-$pod" >> "$log"; rm "more-$pod"; fi
        size=$(wc -c < "$log")
        if [ "$size" -gt "$sent" ]; then tail -c +$((sent + 1)) "$log" | unstamp; sent=$size; fi
        sleep 0.02
    done;;
esac
"#,
        );
        let pod = |n: u64| {
            serde_json::json!({
                "metadata": {
                    "name": format!("pod-{n}"),
                    "uid": format!("uid-{n}"),
                    "resourceVersion": n.to_string(),
                },
                "status": {"containerStatuses": [{"name": "gateway", "restartCount": 0,
                    "ready": true, "state": {"running": {}}}]}
            })
        };
        let set_pod = |n| {
            let tmp = dir.path().join("pod.tmp");
            std::fs::write(&tmp, pod(n).to_string()).unwrap();
            std::fs::rename(tmp, dir.path().join("pod.json")).unwrap();
        };
        let record = |unit: &str, model: &str| {
            let value = serde_json::json!({
                "scope": "request", "wicked_unit": unit, "wicked_run": "run",
                "http.status": 200, "llm_model": model,
            });
            format!("{value}\n")
        };
        for (n, model) in [(1, "A"), (2, "B")] {
            std::fs::write(
                dir.path().join(format!("record-{n}")),
                record("launch@nonce", model),
            )
            .unwrap();
        }
        set_pod(1);
        let gw = Gw {
            kube: Kube {
                kubectl,
                kubeconfig: None,
                context: "test".into(),
                owner: "test".into(),
            },
            ns: "test".into(),
            pod: "pod-1".into(),
            service_ip: String::new(),
            dir: dir.path().into(),
        };
        let arm = gw.log_state().unwrap();
        let overlapping = gw.log_state().unwrap();
        let wait_for_spool = |uid: &str| {
            wait_until("follower did not spool the pod", || {
                std::fs::read_to_string(dir.path().join(format!("records/{uid}.log")))
                    .is_ok_and(|s| s.contains("launch@nonce"))
            });
        };
        wait_for_spool("uid-1");
        set_pod(2);
        std::fs::write(dir.path().join("end-1"), b"").unwrap();
        wait_for_spool("uid-2");
        wait_until("both serving pods were not covered", || {
            gw.records("0", "launch@nonce", Some(&arm)).1
        });
        let (records, complete) = gw.records("0", "launch@nonce", Some(&arm));
        let mut models: Vec<_> = records.iter().filter_map(|r| r.model.as_deref()).collect();
        models.sort_unstable();
        assert_eq!(models, ["A", "B"]);
        assert!(
            complete,
            "both serving pods were followed throughout the lease"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("followers"))
                .unwrap()
                .lines()
                .count(),
            2,
        );
        assert_eq!(
            gw.records("0", "launch@nonce", Some(&overlapping)).0.len(),
            2
        );

        // A later lease restarts a dead follower at the live tail. Earlier windows retain
        // their gap; replay must not duplicate the A/B evidence already in the spool.
        let spool = &arm.lease.as_ref().unwrap().spool;
        spool
            .state
            .lock()
            .unwrap()
            .pods
            .get_mut("uid-2")
            .unwrap()
            .follower
            .as_ref()
            .unwrap()
            .child
            .lock()
            .unwrap()
            .kill()
            .unwrap();
        wait_until("follower did not exit", || {
            !gw.records("0", "launch@nonce", Some(&arm)).1
        });
        let resumed = gw.log_state().unwrap();
        wait_until("follower did not resume", || {
            std::fs::read_to_string(dir.path().join("followers"))
                .unwrap()
                .lines()
                .count()
                >= 3
        });
        std::fs::write(dir.path().join("more.tmp"), record("launch@resumed", "C")).unwrap();
        std::fs::rename(dir.path().join("more.tmp"), dir.path().join("more-2")).unwrap();
        wait_until("resumed stream did not append", || {
            std::fs::read_to_string(dir.path().join("records/uid-2.log"))
                .unwrap()
                .contains("launch@resumed")
        });
        let (records, complete) = gw.records("0", "launch@resumed", Some(&resumed));
        assert_eq!(records.len(), 1);
        assert!(complete);
        let (records, complete) = gw.records("0", "launch@nonce", Some(&arm));
        assert_eq!(records.len(), 2);
        assert!(!complete);

        // A pod first observed after it ended is an explicit coverage gap.
        let mut missed = pod(3);
        missed["status"]["containerStatuses"][0]["state"] = serde_json::json!({"terminated": {}});
        std::fs::write(dir.path().join("pod.tmp"), missed.to_string()).unwrap();
        std::fs::rename(dir.path().join("pod.tmp"), dir.path().join("pod.json")).unwrap();
        wait_until("missed pod event was not observed", || {
            arm.lease
                .as_ref()
                .unwrap()
                .spool
                .state
                .lock()
                .unwrap()
                .pods
                .contains_key("uid-3")
        });
        assert!(!gw.records("0", "launch@nonce", Some(&arm)).1);
        std::fs::write(dir.path().join("end-2"), b"").unwrap();
        drop(arm);
        assert!(dir.path().join("records/uid-1.log").is_file());
        drop(overlapping);
        assert!(!dir.path().join("records/uid-1.log").exists());
        drop(resumed);
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_confirmation_cleans_up_the_same_remote_marker() {
        let dir = TestDir::new();
        let kubectl = dir.path().join("kubectl");
        // (core#2028) Through `install_stub`, as core#1769: this process never holds a writable
        // descriptor to the script it execs, so a first exec never fails with `ETXTBSY`.
        crate::test_env::install_stub(
            &kubectl,
            r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
while [ "$1" != exec ]; do shift; done
shift
while [ "$1" != -- ]; do shift; done
shift
case "$*" in
*pkill*)
    printf '%s\n' "$@" > cleanup
    exit 23
    ;;
*)
    for arg do marker=$arg; done
    printf '%s' "$marker" > marker.tmp && mv marker.tmp marker
    exec sleep 600
    ;;
esac
"#,
        );
        let gw = Gw {
            kube: Kube {
                kubectl,
                kubeconfig: None,
                context: "test".into(),
                owner: "test".into(),
            },
            ns: "test".into(),
            pod: "gateway".into(),
            service_ip: String::new(),
            dir: dir.path().into(),
        };
        let result = gw.exec_marked(
            "wicked-writer-",
            &confirm_script(true),
            Some(b"test-key\n"),
            // core#1757: the stub picks its branch by the script (cleanup or not), never by a
            // file the first exec may not have written yet; the limit only has to outlast the
            // first shell's start on a loaded runner.
            Duration::from_secs(3),
        );
        assert_eq!(result.code, None);
        let cleanup = std::fs::read_to_string(dir.path().join("cleanup"))
            .expect("a timed-out exec must issue a bounded cleanup exec");
        // The bracketed pattern avoids killing the cleanup shell itself.
        let pattern = regex::Regex::new(r"\[w\]icked-writer-[0-9a-f]{32}").unwrap();
        let targeted = pattern
            .find(&cleanup)
            .expect("the cleanup names a writer marker")
            .as_str();
        // The first exec records the marker it was given once its shell runs (it may not have run
        // yet on a loaded runner when the limit fires): when it did, it is the one cleaned up.
        if let Ok(marker) = std::fs::read_to_string(dir.path().join("marker")) {
            assert_eq!(targeted, format!("[{}]{}", &marker[..1], &marker[1..]));
        }
        assert!(cleanup.contains("pkill -9 -f"));
        assert!(cleanup.contains("pgrep -f"));
        assert!(result.err_text().contains("cleanup"), "{result:?}");
    }

    #[test]
    fn log_completeness_requires_the_same_pod_and_all_available_logs_1546() {
        let state = |name: &str, uid: &str, restarts: u64| {
            LogState::from_pod(&serde_json::json!({
                "metadata": {"name": name, "uid": uid},
                "status": {"containerStatuses": [
                    {"name": "writer", "restartCount": 99},
                    {"name": "gateway", "restartCount": restarts}
                ]}
            }))
            .unwrap()
        };
        let arm = state("agw-a", "uid-a", 0);
        assert!(logs_complete(Some(&arm), Some(&arm), false, true));
        assert!(!logs_complete(Some(&arm), Some(&arm), true, false));
        let restarted = state("agw-a", "uid-a", 1);
        assert!(logs_complete(Some(&arm), Some(&restarted), true, true));
        assert!(!logs_complete(Some(&arm), Some(&restarted), false, true));
        assert!(!logs_complete(Some(&arm), Some(&restarted), true, false));
        assert!(!logs_complete(
            Some(&restarted),
            Some(&restarted),
            false,
            true
        ));
        let twice = state("agw-a", "uid-a", 2);
        assert!(!logs_complete(Some(&arm), Some(&twice), true, true));
        assert!(logs_complete(Some(&restarted), Some(&twice), true, true));
        assert!(!logs_complete(Some(&twice), Some(&arm), true, true));
        for replaced in [state("agw-b", "uid-b", 0), state("agw-a", "uid-b", 0)] {
            assert!(!logs_complete(Some(&arm), Some(&replaced), true, true));
        }
        assert!(!logs_complete(None, Some(&arm), true, true));
        assert!(!logs_complete(Some(&arm), None, true, true));
        for pod in [
            serde_json::json!({}),
            serde_json::json!({"metadata": {"name": "agw-a", "uid": "uid-a"}}),
            serde_json::json!({
                "metadata": {"name": "agw-a", "uid": "uid-a"},
                "status": {"containerStatuses": [{"name": "gateway"}]}
            }),
        ] {
            assert!(LogState::from_pod(&pod).is_none());
        }
    }
}
