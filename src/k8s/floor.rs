//! The checks floor on pods (PARITY B13, B14; k8s-swap B-3): the cluster half of
//! [`crate::checks_container::ChecksFloor`].
//!
//! * **Where.** A run's floor pods, fill pods and cache volumes live in the run's own namespace
//!   (its quota, its deny-all policy, deleted with the run). A floor with no run gets a
//!   namespace of its own, deleted when the floor is dropped ([`Place`]).
//! * **Caches are per run** (deviation from PARITY B14, which named read-only local
//!   PersistentVolumes shared across runs). The engine's account has no PersistentVolume verbs
//!   (B-1 review r1: a PV names a node path), and a claim is visible only in its own namespace.
//!   So each run fills its `node_modules` and cargo registry once, in a fill pod with the
//!   network ([`render::FILL_ROLE`], `fill-egress-internet`), and every floor pod and seat pod of
//!   the run mounts the claim read-only. The fill runs exactly what the Docker floor's install
//!   container ran (no repository code). Cross-run reuse is a measured follow-up.
//! * **Exit codes.** Every check runs under a fixed wrapper that writes its exit code to
//!   [`RC_FILE`] after ending what the check left running ([`check_argv`]); the floor takes the
//!   code only when kubectl's agrees with it, so a lost exec stream is never a check's result
//!   ([`rc_verdict`]).

use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::render::{self, FloorPod};
use super::{cluster, label_value, Kube};

/// The container every floor, fill and probe pod runs its work in.
pub(crate) const CONTAINER: &str = "checks";
/// The in-memory dir the check wrapper writes its exit code to.
pub(crate) const RC_DIR: &str = "/wicked-rc";
/// The check wrapper's exit-code file.
pub(crate) const RC_FILE: &str = "/wicked-rc/rc";
/// How long a floor or fill pod may take to be admitted and start.
pub(crate) const POD_START: Duration = Duration::from_secs(120);

/// Where a floor's pods go: the run's namespace, or (no run) one of the floor's own.
#[derive(Debug)]
pub(crate) struct Place {
    pub(crate) kube: Kube,
    pub(crate) ns: String,
    pub(crate) run: String,
    /// The namespace is this floor's alone: deleted when the floor is dropped.
    pub(crate) owned: bool,
}

impl Place {
    /// The run namespace of `run_id` (created when absent), or a namespace of this floor's own.
    pub(crate) fn for_floor(kube: Kube, run_id: Option<&str>) -> Result<Place, String> {
        let (run, owned) = match run_id.filter(|r| !r.is_empty() && *r != "no-run") {
            Some(r) => (r.to_string(), false),
            None => (
                format!("floor-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
                true,
            ),
        };
        let ns = cluster::ensure_run(&kube, &run, &render::RunBudget::default(), "20Gi")?;
        Ok(Place {
            kube,
            ns,
            run,
            owned,
        })
    }

    /// A pinned kubectl command in this place's namespace.
    pub(crate) fn cmd(&self) -> Command {
        self.kube.cmd(Some(&self.ns))
    }

    /// `kubectl exec [-i] -c checks <pod> --` (the caller appends the argv).
    pub(crate) fn exec(&self, pod: &str, stdin: bool) -> Command {
        let mut c = self.cmd();
        c.arg("exec");
        if stdin {
            c.arg("-i");
        }
        c.args(["-c", CONTAINER, pod, "--"]);
        c
    }

    /// Run `argv` in `pod`, bounded: its stdout on success, else the reason.
    pub(crate) fn run(&self, pod: &str, argv: &[&str], limit: Duration) -> Result<String, String> {
        let mut a: Vec<&str> = vec!["exec", "-c", CONTAINER, pod, "--"];
        a.extend(argv);
        let r = self.kube.call(Some(&self.ns), &a, None, limit);
        if r.ok() {
            Ok(String::from_utf8_lossy(&r.stdout).into_owned())
        } else {
            Err(r.err_text())
        }
    }

    /// The pod spec's common facts for this place.
    pub(crate) fn pod(&self, name: &str, role: &str, image: &str, floor_image: &str) -> FloorPod {
        FloorPod {
            name: name.to_string(),
            namespace: self.ns.clone(),
            owner: self.kube.owner.clone(),
            run_label: label_value(&self.run),
            role: role.to_string(),
            image: image.to_string(),
            floor_image: floor_image.to_string(),
            memory_limit: "1Gi".into(),
            memory_request: "256Mi".into(),
            env: Vec::new(),
            empty: Vec::new(),
            claims: Vec::new(),
            mkdirs: Vec::new(),
            deadline_secs: 6 * 60 * 60,
        }
    }

    /// Prove the running pod `name` has no network (it has no egress policy of its own): retried
    /// while the namespace's deny-all lands. `Err` = it does not hold here.
    pub(crate) fn prove_offline(&self, name: &str) -> Result<(), String> {
        let host_ip = self
            .kube
            .get_json(Some(&self.ns), "pod", name)
            .ok()
            .flatten()
            .and_then(|p| {
                p.pointer("/status/hostIP")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|ip| {
                ip.chars()
                    .all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':')
            });
        let script = offline_script(host_ip.as_deref());
        let start = Instant::now();
        let mut last = String::new();
        while start.elapsed() < Duration::from_secs(20) {
            match self.run(
                name,
                &["/floor/busybox", "sh", "-c", &script],
                Duration::from_secs(15),
            ) {
                Ok(out)
                    if out.contains("net=closed")
                        && (host_ip.is_none() || out.contains("node=closed")) =>
                {
                    return Ok(());
                }
                Ok(out) => last = out.replace('\n', " "),
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(format!(
            "the floor pod is not offline ({}); the checks do not run here",
            last.trim()
        ))
    }

    /// Create `spec`'s pod and wait until it runs. A pod that does not start is deleted, and
    /// the reason names what held it (an image, a policy, the quota).
    pub(crate) fn start(&self, spec: &FloorPod) -> Result<(), String> {
        let doc = render::floor_pod(spec)?;
        if let Err(e) = self.kube.apply(&[doc]) {
            self.delete(&spec.name);
            return Err(format!("the {} pod was not admitted: {e}", spec.role));
        }
        let start = Instant::now();
        let mut last = String::new();
        while start.elapsed() < POD_START {
            match self.kube.get_json(Some(&self.ns), "pod", &spec.name) {
                Ok(Some(p)) => {
                    if cluster::running_pod(std::slice::from_ref(&p)).is_some() {
                        return Ok(());
                    }
                    if let Some(why) = stuck(&p) {
                        self.delete(&spec.name);
                        return Err(format!("the {} pod cannot start: {why}", spec.role));
                    }
                    last = p
                        .pointer("/status/phase")
                        .and_then(Value::as_str)
                        .unwrap_or("Pending")
                        .to_string();
                }
                Ok(None) => last = "absent".into(),
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        self.delete(&spec.name);
        Err(format!(
            "the {} pod did not start within {POD_START:?} (last seen: {last})",
            spec.role
        ))
    }

    /// Delete pod `name` (this daemon's only), without waiting.
    pub(crate) fn delete(&self, name: &str) {
        if let Err(e) = self.kube.delete_owned(Some(&self.ns), "pod", name, false) {
            eprintln!(
                "wicked-core: floor pod {name} in {} not deleted: {e}",
                self.ns
            );
        }
    }

    /// Ensure the claim `name` exists in this place (idempotent).
    pub(crate) fn claim(&self, name: &str, size: &str) -> Result<(), String> {
        self.kube
            .apply(&[render::pvc(&self.ns, &self.kube.owner, name, size)])
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        if self.owned {
            if let Err(e) = cluster::delete_run(&self.kube, &self.run, false) {
                eprintln!(
                    "wicked-core: the floor namespace {} was not deleted: {e}",
                    self.ns
                );
            }
        }
    }
}

/// Why a pending pod will never start on its own (pure): an image the node does not have
/// (`imagePullPolicy: Never`), or a container that failed to be created.
pub(crate) fn stuck(pod: &Value) -> Option<String> {
    let list = |p: &str| {
        pod.pointer(p)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    // An init container that ran to exit 0 is a step done, not an ending.
    let init = list("/status/initContainerStatuses");
    let main = list("/status/containerStatuses");
    for (s, is_init) in init
        .iter()
        .map(|s| (s, true))
        .chain(main.iter().map(|s| (s, false)))
    {
        if let Some(w) = s.pointer("/state/waiting") {
            let reason = w.get("reason").and_then(Value::as_str).unwrap_or("");
            if matches!(
                reason,
                "ErrImageNeverPull"
                    | "ErrImagePull"
                    | "ImagePullBackOff"
                    | "InvalidImageName"
                    | "CreateContainerConfigError"
                    | "CreateContainerError"
                    | "RunContainerError"
            ) {
                let msg = w.get("message").and_then(Value::as_str).unwrap_or("");
                return Some(format!("{reason} {msg}").trim().to_string());
            }
        }
        if let Some(t) = s.pointer("/state/terminated") {
            let code = t.get("exitCode").and_then(Value::as_i64).unwrap_or(-1);
            if is_init && code == 0 {
                continue;
            }
            let reason = t
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("terminated");
            return Some(format!("a container ended ({reason}, exit {code})"));
        }
    }
    match pod.pointer("/status/phase").and_then(Value::as_str) {
        Some("Failed") | Some("Succeeded") => Some("the pod ended".into()),
        _ => None,
    }
}

/// The argv a check runs as inside its floor pod (pure): the fixed wrapper enters `cwd`, runs
/// `env` + `argv`, ends everything the check left running (`kill -9 -1` spares the pod's PID 1
/// and the wrapper) and writes the check's exit code to its own file, `rc-<id>` under [`RC_DIR`]
/// (review r1: one attempt's code is never read as another's). `env` rides `env(1)`'s argv
/// after `--`: the floor's environment carries no secret.
pub(crate) fn check_argv(
    id: &str,
    cwd: &str,
    env: &[(String, String)],
    argv: &[String],
) -> Vec<String> {
    let mut v: Vec<String> = [
        "/bin/sh",
        "-c",
        concat!(
            "id=$1; cd \"$2\" || exit 126; shift 2; \"$@\"; rc=$?; ",
            "kill -9 -1 2>/dev/null; echo \"$rc\" > /wicked-rc/rc-$id; exit $rc"
        ),
        "wicked-check",
    ]
    .map(String::from)
    .to_vec();
    v.push(id.to_string());
    v.push(cwd.to_string());
    v.push("/usr/bin/env".into());
    v.push("--".into());
    v.extend(env.iter().map(|(k, val)| format!("{k}={val}")));
    v.extend(argv.iter().cloned());
    v
}

/// A fresh id for one check's exit-code file (lowercase hex).
pub(crate) fn rc_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// The in-pod script that proves a floor pod offline before any repository code runs in it
/// (pure): the internet and the pod's own node (its kubelet port, which listens on every node
/// address) must not answer (review r1: NetworkPolicy does not portably cover node traffic).
pub(crate) fn offline_script(host_ip: Option<&str>) -> String {
    let node = host_ip
        .map(|ip| format!("nc_ {ip} 10250 node & "))
        .unwrap_or_default();
    format!(
        "nc_() {{ if /floor/busybox nc -w 2 \"$1\" \"$2\" </dev/null >/dev/null 2>&1; then echo \"$3=open\"; else echo \"$3=closed\"; fi; }}; \
         nc_ 1.1.1.1 443 net & {node}wait"
    )
}

/// Whether the check's exit code as kubectl reported it (`seen`) is the check's own, given what
/// the wrapper wrote (`rc`; `None` = no file) (pure). `Some(reason)` = the stream was lost: no
/// exit code is evidence then. A check that ran past its deadline or was ended by a signal has
/// no code to compare (`seen` = `None`), and is judged by its caller.
pub(crate) fn rc_verdict(seen: Option<i32>, rc: Option<&str>) -> Option<String> {
    let seen = seen?;
    match rc.map(str::trim).and_then(|r| r.parse::<i32>().ok()) {
        Some(r) if r == seen => None,
        Some(r) => Some(format!(
            "floor infrastructure: the exec stream was lost (kubectl said {seen}, the check \
             exited {r}) — not a failure of the change"
        )),
        None => Some(format!(
            "floor infrastructure: the exec stream was lost before the check finished (kubectl \
             said {seen}, the check left no exit code) — not a failure of the change"
        )),
    }
}
