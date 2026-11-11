//! Pure renderers: every manifest the Kubernetes runner applies, as JSON. Nothing here runs
//! anything, so each hardening and isolation property is pinned by a test (`super::tests`).

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::{MANAGED_BY, MANAGED_LABEL, OWNER_LABEL, ROLE_LABEL, RUN_LABEL};

/// The uid:gid every engine pod runs as (seats, floors, sync, fill). Pods never share a
/// process namespace, so one uid across pods grants nothing across them.
pub(crate) const POD_UID: i64 = 1000;
/// The gateway's uid (agentgateway's own, as the container floor ran it).
pub(crate) const GATEWAY_UID: i64 = 10001;
/// Where the trusted launcher's files land in a seat or floor pod (read-only).
pub(crate) const FLOOR_DIR: &str = "/floor";
/// The in-memory directory the engine writes one unit's secrets into (its env file, a long
/// prompt), read by the launcher before Landlock and outside every Landlock grant after it.
pub(crate) const UNIT_DIR: &str = "/wicked-unit";
/// The PVC holding a run's replica (its trees, git state and the unit's admitted roots, each
/// at its host path).
pub(crate) const REPLICA_PVC: &str = "replica";
/// The pod's own PID 1: a busybox shell loop. As its PID namespace's init it ignores every
/// signal sent from inside the pod (a seat's or a check's `kill -9 -1` leaves it running, and the
/// pod with it), and each `sleep` it waits for reaps any orphan that exited (no zombies hold the
/// pids limit). Measured on the cluster (lane data/init-probe.txt). No workload ever starts at
/// container start (PARITY F1): the CLI starts only by exec, after the deny check.
pub(crate) const IDLE_COMMAND: &[&str] = &[
    "/floor/busybox",
    "sh",
    "-c",
    "trap '' TERM INT HUP; while :; do /floor/busybox sleep 2; done",
];
/// The gateway's listeners a seat may reach (see `container_floor::GATEWAY_PORTS`).
pub(crate) fn seat_gateway_ports() -> &'static [u16] {
    crate::container_floor::GATEWAY_PORTS
}

/// Private and link-local ranges: never a destination for a pod that has internet egress
/// (the gateway, a cache fill). The cluster's own pod and service ranges are inside them.
pub(crate) const PRIVATE_RANGES: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "100.64.0.0/10",
    "127.0.0.0/8",
];

/// A kubectl `List` of `docs`, applied as one document.
pub(crate) fn list(docs: &[Value]) -> Value {
    json!({"apiVersion": "v1", "kind": "List", "items": docs})
}

/// The labels every engine object carries: its owner and the managed-by label, plus `extra`.
pub(crate) fn labels(owner: &str, extra: &[(&str, &str)]) -> Value {
    let mut m = Map::new();
    m.insert(OWNER_LABEL.into(), json!(owner));
    m.insert(MANAGED_LABEL.into(), json!(MANAGED_BY));
    for (k, v) in extra {
        m.insert((*k).into(), json!(v));
    }
    Value::Object(m)
}

fn meta(name: &str, ns: Option<&str>, owner: &str, extra: &[(&str, &str)]) -> Value {
    let mut m = json!({"name": name, "labels": labels(owner, extra)});
    if let Some(ns) = ns {
        m["namespace"] = json!(ns);
    }
    m
}

/// A namespace under PodSecurity `restricted`, enforced (and audited, warned) at the latest
/// version: no pod that could escalate, mount the host or run as root is admitted in it.
pub(crate) fn namespace(name: &str, owner: &str, extra: &[(&str, &str)]) -> Value {
    let mut md = meta(name, None, owner, extra);
    let l = md["labels"].as_object_mut().expect("labels object");
    for mode in ["enforce", "audit", "warn"] {
        l.insert(
            format!("pod-security.kubernetes.io/{mode}"),
            json!("restricted"),
        );
        l.insert(
            format!("pod-security.kubernetes.io/{mode}-version"),
            json!("latest"),
        );
    }
    json!({"apiVersion": "v1", "kind": "Namespace", "metadata": md})
}

/// The admission budget of one run namespace (PARITY B16): pods beyond it wait as Pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunBudget {
    pub(crate) requests_memory: String,
    pub(crate) requests_cpu: String,
    pub(crate) pods: u32,
}

impl Default for RunBudget {
    /// A step seat, two judges, two floor sides and the sync pod at their requests, with room
    /// for a team member (re-measured after B-2, coord 2026-11-11).
    fn default() -> Self {
        RunBudget {
            requests_memory: "8Gi".into(),
            requests_cpu: "4".into(),
            pods: 16,
        }
    }
}

pub(crate) fn resource_quota(ns: &str, owner: &str, b: &RunBudget) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ResourceQuota",
        "metadata": meta("run-budget", Some(ns), owner, &[]),
        "spec": {"hard": {
            "requests.memory": b.requests_memory,
            "requests.cpu": b.requests_cpu,
            "pods": b.pods.to_string(),
            "services.loadbalancers": "0",
            "services.nodeports": "0",
        }}
    })
}

/// Defaults for a container that names no resources: it is admitted with small requests and a
/// limit instead of running unbounded.
pub(crate) fn limit_range(ns: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "LimitRange",
        "metadata": meta("defaults", Some(ns), owner, &[]),
        "spec": {"limits": [{
            "type": "Container",
            "default": {"memory": "512Mi"},
            "defaultRequest": {"memory": "64Mi", "cpu": "10m"},
        }]}
    })
}

/// Deny every pod of `ns` all ingress and all egress: what is allowed is added per role.
pub(crate) fn default_deny(ns: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": meta("default-deny", Some(ns), owner, &[]),
        "spec": {"podSelector": {}, "policyTypes": ["Ingress", "Egress"]}
    })
}

/// The roles that may reach the gateway: seats and judges. Floors, sync and fill pods never.
pub(crate) const GATEWAY_CLIENT_ROLES: &[&str] = &["seat", "judge"];

/// The label saying whether a seat or judge pod may reach the gateway (`online`) or runs with no
/// network at all (`offline`, `WICKED_SEAT_NETWORK=none`): both gateway policies select
/// `online` only (review r4: an offline seat could reach a running gateway's CONNECT route by IP).
pub(crate) const NET_LABEL: &str = "wicked.dev/net";

/// Seats' and judges' only egress: the gateway pods of the daemon namespace, on the seat ports.
/// No DNS: a seat reaches the gateway by `hostAliases` (closing DNS as a tunnel).
pub(crate) fn seat_egress(ns: &str, owner: &str, daemon_ns: &str, ports: &[u16]) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": meta("seat-egress-gateway-only", Some(ns), owner, &[]),
        "spec": {
            "podSelector": {
                "matchLabels": {NET_LABEL: "online"},
                "matchExpressions": [{
                    "key": ROLE_LABEL, "operator": "In", "values": GATEWAY_CLIENT_ROLES,
                }],
            },
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{
                    "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": daemon_ns}},
                    "podSelector": {"matchLabels": {ROLE_LABEL: "gateway"}},
                }],
                "ports": tcp_ports(ports),
            }]
        }
    })
}

fn tcp_ports(ports: &[u16]) -> Vec<Value> {
    ports
        .iter()
        .map(|p| json!({"protocol": "TCP", "port": p}))
        .collect()
}

/// In the daemon namespace: the gateway admits seats and judges of THIS daemon's run
/// namespaces only, on the seat ports (never its readiness port).
pub(crate) fn gateway_ingress(daemon_ns: &str, owner: &str, ports: &[u16]) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": meta("gateway-ingress-seats", Some(daemon_ns), owner, &[]),
        "spec": {
            "podSelector": {"matchLabels": {ROLE_LABEL: "gateway"}},
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": {"matchLabels": {
                        OWNER_LABEL: owner, ROLE_LABEL: "run",
                    }},
                    "podSelector": {
                        "matchLabels": {NET_LABEL: "online"},
                        "matchExpressions": [{
                            "key": ROLE_LABEL, "operator": "In", "values": GATEWAY_CLIENT_ROLES,
                        }],
                    },
                }],
                "ports": tcp_ports(ports),
            }]
        }
    })
}

/// An upstream the gateway may reach on the host (the engine's gate-hook relay, crew's MCP
/// broker), as the pod reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPort {
    pub(crate) ip: String,
    pub(crate) port: u16,
}

/// The gateway's egress: the internet (model APIs, the CONNECT allowlist's hosts) but no
/// private or cluster range; cluster DNS (it resolves upstream names; seats do not); and the
/// named host ports.
pub(crate) fn gateway_egress(daemon_ns: &str, owner: &str, host: &[HostPort]) -> Value {
    let mut egress = vec![
        json!({"to": [{"ipBlock": {"cidr": "0.0.0.0/0", "except": PRIVATE_RANGES}}],
               "ports": [{"protocol": "TCP", "port": 443}, {"protocol": "TCP", "port": 80}]}),
        dns_rule(),
    ];
    for h in host {
        egress.push(json!({
            "to": [{"ipBlock": {"cidr": format!("{}/32", h.ip)}}],
            "ports": [{"protocol": "TCP", "port": h.port}],
        }));
    }
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": meta("gateway-egress", Some(daemon_ns), owner, &[]),
        "spec": {
            "podSelector": {"matchLabels": {ROLE_LABEL: "gateway"}},
            "policyTypes": ["Egress"],
            "egress": egress,
        }
    })
}

fn dns_rule() -> Value {
    json!({
        "to": [{
            "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "kube-system"}},
            "podSelector": {"matchLabels": {"k8s-app": "kube-dns"}},
        }],
        "ports": [{"protocol": "UDP", "port": 53}, {"protocol": "TCP", "port": 53}],
    })
}

/// A cache fill's egress (it alone has the network, and runs no repository code): the internet
/// on https and http, and DNS; never a private or cluster range.
pub(crate) fn fill_egress(ns: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": meta("fill-egress-internet", Some(ns), owner, &[]),
        "spec": {
            "podSelector": {"matchLabels": {ROLE_LABEL: "fill"}},
            "policyTypes": ["Egress"],
            "egress": [
                {"to": [{"ipBlock": {"cidr": "0.0.0.0/0", "except": PRIVATE_RANGES}}],
                 "ports": [{"protocol": "TCP", "port": 443}, {"protocol": "TCP", "port": 80}]},
                dns_rule(),
            ],
        }
    })
}

/// A PersistentVolumeClaim on the cluster's default storage class.
pub(crate) fn pvc(ns: &str, owner: &str, name: &str, size: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": meta(name, Some(ns), owner, &[]),
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "resources": {"requests": {"storage": size}},
        }
    })
}

/// The restricted container security context every engine container runs with.
pub(crate) fn hardened_context() -> Value {
    json!({
        "allowPrivilegeEscalation": false,
        "readOnlyRootFilesystem": true,
        "privileged": false,
        "capabilities": {"drop": ["ALL"]},
        "seccompProfile": {"type": "RuntimeDefault"},
    })
}

fn pod_context(uid: i64) -> Value {
    json!({
        "runAsNonRoot": true,
        "runAsUser": uid,
        "runAsGroup": uid,
        "fsGroup": uid,
        "fsGroupChangePolicy": "OnRootMismatch",
        "seccompProfile": {"type": "RuntimeDefault"},
    })
}

/// One path of a run's replica mounted into a seat pod at its host path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplicaMount {
    pub(crate) path: PathBuf,
    pub(crate) rw: bool,
}

/// The replica volume's sub-path for a host path: the path without its root.
pub(crate) fn replica_sub_path(p: &Path) -> Option<String> {
    let rel = p.strip_prefix("/").ok()?;
    let s = rel.to_str()?;
    (!s.is_empty() && !s.split('/').any(|c| c == ".." || c == "." || c.is_empty()))
        .then(|| s.to_string())
}

/// Everything one seat (or judge) pod is created with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatPod {
    pub(crate) name: String,
    pub(crate) namespace: String,
    pub(crate) owner: String,
    pub(crate) run_label: String,
    pub(crate) unit_label: String,
    pub(crate) boot: String,
    /// `seat` or `judge`.
    pub(crate) role: String,
    pub(crate) image: String,
    pub(crate) floor_image: String,
    pub(crate) workdir: PathBuf,
    pub(crate) mounts: Vec<ReplicaMount>,
    pub(crate) memory_limit: String,
    pub(crate) memory_request: String,
    pub(crate) cpu_request: String,
    /// The gateway Service's address, resolved in the pod as `agw`.
    pub(crate) gateway_ip: Option<String>,
    /// RFC 3339: past it the controller deletes the sandbox even if the engine is gone.
    pub(crate) shutdown_time: String,
    /// The pod's own deadline (`activeDeadlineSeconds`): the kubelet kills it past this, even
    /// with no controller or API server left.
    pub(crate) deadline_secs: u64,
    /// The replica sub-directory every path of this pod lives under (a one-shot engine session's
    /// own scope in the shared session namespace); `None` = the replica's root.
    pub(crate) scope: Option<String>,
    /// PARITY B17: a read-only unit's tree. The replica's copy is mounted read-only at
    /// [`VIEW_SOURCE`], and an init container copies it into an emptyDir at the tree's own
    /// path: the unit's writes land in that throwaway view and go with the pod.
    pub(crate) view: Option<PathBuf>,
    /// PARITY B14: the run's `node_modules` cache claim, mounted read-only at
    /// `<tree>/node_modules` (`(claim, tree)`), filled once per run by a fill pod.
    pub(crate) deps: Option<(String, PathBuf)>,
}

/// Where a read-only unit's replica tree is mounted read-only (the view's source).
pub(crate) const VIEW_SOURCE: &str = "/wicked-ro";

/// The replica sub-path of `p` under an optional `scope`.
pub(crate) fn scoped_sub_path(scope: Option<&str>, p: &Path) -> Option<String> {
    let sub = replica_sub_path(p)?;
    Some(match scope {
        Some(s) => format!("{s}/{sub}"),
        None => sub,
    })
}

/// The dir inside a read-only unit's view emptyDir that holds the tree, mounted at the tree's
/// path by `subPath`. The emptyDir's own root is the kubelet's (`0:<fsGroup>`), so git in the
/// pod refused a tree mounted there ("dubious ownership", phase C); this dir is made by the
/// view init as the pod's uid, so the tree root is the seat's own, as on the replica.
pub(crate) const VIEW_TREE: &str = "tree";

/// The init container that copies a read-only unit's replica tree into its view (no link
/// followed; tracked content under any name, `target/` included, review r1).
fn view_init(floor_image: &str, sub: &str) -> Value {
    json!({
        "name": "view",
        "image": floor_image,
        "imagePullPolicy": "Never",
        "command": ["/busybox", "sh", "-c",
            // Both sides' failure fails the init (codex b2-gw #6): a copy cut short never
            // starts the reviewer on part of the tree.
            // Made afresh each time (codex r1 #11): a re-run init never fails on, or keeps, the
            // tree an earlier run left.
            format!("set -o pipefail; /busybox rm -rf /wicked-view/{VIEW_TREE} && \
                     /busybox mkdir /wicked-view/{VIEW_TREE} && cd /wicked-ro && \
                     /busybox tar -cf - . | /busybox tar -C /wicked-view/{VIEW_TREE} -xf -")],
        "securityContext": hardened_context(),
        "resources": {"requests": {"memory": "32Mi", "cpu": "50m"}, "limits": {"memory": "256Mi"}},
        "volumeMounts": [
            {"name": "replica", "mountPath": VIEW_SOURCE, "subPath": sub, "readOnly": true},
            {"name": "view", "mountPath": "/wicked-view"},
        ],
    })
}

/// The init container that copies the engine's trusted launcher files out of the floor image
/// into the pod's read-only `/floor` (a cri-dockerd pod has no image volume).
fn floor_init(floor_image: &str) -> Value {
    json!({
        "name": "floor",
        "image": floor_image,
        "imagePullPolicy": "Never",
        "command": ["/busybox", "cp", "/launch", "/busybox", "/landrun", "/floor/"],
        "securityContext": hardened_context(),
        "resources": {"requests": {"memory": "16Mi", "cpu": "10m"}, "limits": {"memory": "64Mi"}},
        "volumeMounts": [{"name": "floor", "mountPath": FLOOR_DIR}],
    })
}

/// A seat's pod template: the seat image idling under the engine's PID 1 loop, every replica path
/// at its host path with the mount plan's access, in-memory canary and unit dirs, and no
/// service-account token, service links or host namespaces.
pub(crate) fn seat_pod_spec(s: &SeatPod) -> Result<Value, String> {
    let mut mounts = vec![
        json!({"name": "floor", "mountPath": FLOOR_DIR, "readOnly": true}),
        json!({"name": "tmp", "mountPath": "/tmp"}),
        json!({"name": "home", "mountPath": "/home/seat"}),
        json!({"name": "canary", "mountPath": "/wicked-canary"}),
        json!({"name": "unit", "mountPath": UNIT_DIR}),
    ];
    let reserved = [
        FLOOR_DIR,
        "/tmp",
        "/home/seat",
        "/wicked-canary",
        UNIT_DIR,
        VIEW_SOURCE,
    ];
    let mut init = vec![floor_init(&s.floor_image)];
    let mut extra_volumes = Vec::new();
    if let Some(tree) = &s.view {
        let sub = scoped_sub_path(s.scope.as_deref(), tree)
            .ok_or_else(|| format!("{} cannot be a replica path", tree.display()))?;
        mounts.push(
            json!({"name": "replica", "mountPath": VIEW_SOURCE, "subPath": sub, "readOnly": true}),
        );
        mounts.push(
            json!({"name": "view", "mountPath": tree.to_string_lossy(), "subPath": VIEW_TREE}),
        );
        init.push(view_init(&s.floor_image, &sub));
        extra_volumes.push(json!({"name": "view", "emptyDir": {"sizeLimit": "8Gi"}}));
    }
    // Shallower first, so a deeper mount lands over its parent.
    let mut plan: Vec<&ReplicaMount> = s.mounts.iter().collect();
    plan.sort_by_key(|m| m.path.components().count());
    for m in plan {
        if s.view.as_ref() == Some(&m.path) {
            continue;
        }
        let sub = scoped_sub_path(s.scope.as_deref(), &m.path)
            .ok_or_else(|| format!("{} cannot be a replica path", m.path.display()))?;
        // A path under `/tmp` may be mounted over the pod's scratch (an engine session's tree
        // lives there on Linux, review r1); `/tmp` itself and every other reserved path may not.
        if reserved
            .iter()
            .any(|r| Path::new(r).starts_with(&m.path) || (*r != "/tmp" && m.path.starts_with(r)))
        {
            return Err(format!(
                "{} would cover a path the pod reserves",
                m.path.display()
            ));
        }
        mounts.push(json!({
            "name": "replica",
            "mountPath": m.path.to_string_lossy(),
            "subPath": sub,
            "readOnly": !m.rw,
        }));
    }
    let mut spec = json!({
        "automountServiceAccountToken": false,
        "enableServiceLinks": false,
        "hostNetwork": false,
        "hostPID": false,
        "hostIPC": false,
        "restartPolicy": "Never",
        "terminationGracePeriodSeconds": 1,
        // Kubelet-enforced, independent of the controller and the engine (review r1 P2).
        "activeDeadlineSeconds": s.deadline_secs,
        "securityContext": pod_context(POD_UID),
        "initContainers": init,
        "containers": [{
            "name": "seat",
            "image": s.image,
            "imagePullPolicy": "Never",
            "command": IDLE_COMMAND,
            "workingDir": s.workdir.to_string_lossy(),
            "env": [
                {"name": "HOME", "value": "/home/seat"},
                {"name": "TMPDIR", "value": "/tmp"},
                {"name": "LANG", "value": "C.UTF-8"},
            ],
            "securityContext": hardened_context(),
            "resources": {
                "requests": {"memory": s.memory_request, "cpu": s.cpu_request},
                "limits": {"memory": s.memory_limit},
            },
            "volumeMounts": mounts,
        }],
        "volumes": [
            {"name": "floor", "emptyDir": {"sizeLimit": "64Mi"}},
            // In memory, like the container floor's tmpfs: charged to the seat's memory limit, so
            // a seat cannot fill the node's disk through them (review r1 P2).
            {"name": "tmp", "emptyDir": {"medium": "Memory", "sizeLimit": "2Gi"}},
            {"name": "home", "emptyDir": {"medium": "Memory", "sizeLimit": "512Mi"}},
            {"name": "canary", "emptyDir": {"medium": "Memory", "sizeLimit": "1Mi"}},
            {"name": "unit", "emptyDir": {"medium": "Memory", "sizeLimit": "1Mi"}},
            {"name": "replica", "persistentVolumeClaim": {"claimName": REPLICA_PVC}},
        ],
    });
    if let Some((claim, tree)) = &s.deps {
        let at = tree.join("node_modules");
        if let Some(m) = spec
            .pointer_mut("/containers/0/volumeMounts")
            .and_then(Value::as_array_mut)
        {
            m.push(json!({"name": "deps", "mountPath": at.to_string_lossy(), "readOnly": true}));
        }
        extra_volumes.push(json!({"name": "deps",
            "persistentVolumeClaim": {"claimName": claim, "readOnly": true}}));
    }
    if let Some(v) = spec["volumes"].as_array_mut() {
        v.extend(extra_volumes);
    }
    if let Some(ip) = &s.gateway_ip {
        spec["hostAliases"] = json!([{"ip": ip, "hostnames": ["agw"]}]);
    }
    Ok(spec)
}

/// The agent-sandbox `Sandbox` one seat (or a run's judge) runs in: a single pod, never
/// re-created when it fails, no Service, and deleted by the controller at `shutdown_time` even
/// if the engine is gone.
pub(crate) fn seat_sandbox(s: &SeatPod) -> Result<Value, String> {
    let pod_labels = labels(
        &s.owner,
        &[
            (RUN_LABEL, &s.run_label),
            (super::UNIT_LABEL, &s.unit_label),
            (super::BOOT_LABEL, &s.boot),
            (ROLE_LABEL, &s.role),
            (
                NET_LABEL,
                if s.gateway_ip.is_some() {
                    "online"
                } else {
                    "offline"
                },
            ),
        ],
    );
    Ok(json!({
        "apiVersion": "agents.x-k8s.io/v1beta1",
        "kind": "Sandbox",
        "metadata": {
            "name": s.name,
            "namespace": s.namespace,
            "labels": pod_labels.clone(),
        },
        "spec": {
            "failedPodPolicy": "Retain",
            "shutdownPolicy": "Delete",
            "shutdownTime": s.shutdown_time,
            "service": false,
            "podTemplate": {
                "metadata": {"labels": pod_labels},
                "spec": seat_pod_spec(s)?,
            },
        }
    }))
}

/// Everything the daemon's gateway is created with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GatewaySpec {
    pub(crate) namespace: String,
    pub(crate) owner: String,
    pub(crate) image: String,
    pub(crate) writer_image: String,
    /// The gateway's boot token: its container starts only once the writer has published a
    /// config AND this token to `/cfg/boot` (review r1: a gateway re-created after a quarantine
    /// must never serve the config it was quarantined with).
    pub(crate) boot: String,
}

pub(crate) const GATEWAY_NAME: &str = "agw";
/// The gateway's config and broker volumes.
pub(crate) const GATEWAY_CONFIG_PVC: &str = "gateway-config";
pub(crate) const GATEWAY_BROKER_PVC: &str = "gateway-broker";

/// The gateway: agentgateway reading its config and the broker read-only, beside one long-lived
/// `writer` container (the only one that mounts them read-write): config publishes, credential
/// writes, readiness and key probes are execs into it (PARITY B8, B9, core#1394), never helper
/// containers. Seats reach it through the `agw` Service on the seat ports.
pub(crate) fn gateway_deployment(g: &GatewaySpec) -> Value {
    let pod_labels = labels(&g.owner, &[(ROLE_LABEL, "gateway")]);
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": meta(GATEWAY_NAME, Some(&g.namespace), &g.owner, &[(ROLE_LABEL, "gateway")]),
        "spec": {
            "replicas": 1,
            "strategy": {"type": "Recreate"},
            "selector": {"matchLabels": {super::OWNER_LABEL: g.owner, ROLE_LABEL: "gateway"}},
            "template": {
                "metadata": {"labels": pod_labels},
                "spec": {
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "securityContext": pod_context(GATEWAY_UID),
                    "terminationGracePeriodSeconds": 5,
                    // The writer is a native sidecar (an init container that keeps running):
                    // it is up before the gateway, so the engine can publish the first config
                    // into it; `wait-config` holds the gateway until that config exists
                    // (agentgateway's image has no shell to wait with).
                    "initContainers": [
                        {
                            "name": "writer",
                            "image": g.writer_image,
                            "imagePullPolicy": "IfNotPresent",
                            "restartPolicy": "Always",
                            "command": ["sleep", "2147483647"],
                            "securityContext": hardened_context(),
                            "resources": {
                                "requests": {"memory": "8Mi", "cpu": "5m"},
                                "limits": {"memory": "64Mi"},
                            },
                            "volumeMounts": [
                                {"name": "cfg", "mountPath": "/cfg"},
                                {"name": "broker", "mountPath": "/broker"},
                                {"name": "tmp", "mountPath": "/tmp"},
                            ],
                        },
                        {
                            "name": "wait-config",
                            "image": g.writer_image,
                            "imagePullPolicy": "IfNotPresent",
                            "command": ["sh", "-c", "until [ -s /cfg/config.json ] && [ \"$(cat /cfg/boot 2>/dev/null)\" = \"$WICKED_GW_BOOT\" ]; do sleep 0.2; done"],
                            "env": [{"name": "WICKED_GW_BOOT", "value": g.boot}],
                            "securityContext": hardened_context(),
                            "resources": {
                                "requests": {"memory": "8Mi", "cpu": "5m"},
                                "limits": {"memory": "32Mi"},
                            },
                            "volumeMounts": [{"name": "cfg", "mountPath": "/cfg", "readOnly": true}],
                        },
                    ],
                    "containers": [{
                        "name": "gateway",
                        "image": g.image,
                        "imagePullPolicy": "IfNotPresent",
                        "args": ["-f", "/cfg/config.json"],
                        // Ready only once agentgateway listens (review r1 P2).
                        "readinessProbe": {
                            "tcpSocket": {"port": crate::seat_gateway::READY_PORT},
                            "periodSeconds": 1,
                            "failureThreshold": 3,
                        },
                        "securityContext": hardened_context(),
                        "resources": {
                            "requests": {"memory": "128Mi", "cpu": "50m"},
                            "limits": {"memory": "512Mi"},
                        },
                        "volumeMounts": [
                            {"name": "cfg", "mountPath": "/cfg", "readOnly": true},
                            {"name": "broker", "mountPath": "/broker", "readOnly": true},
                        ],
                    }],
                    "volumes": [
                        {"name": "cfg", "persistentVolumeClaim": {"claimName": GATEWAY_CONFIG_PVC}},
                        {"name": "broker", "persistentVolumeClaim": {"claimName": GATEWAY_BROKER_PVC}},
                        {"name": "tmp", "emptyDir": {"medium": "Memory", "sizeLimit": "16Mi"}},
                    ],
                },
            },
        }
    })
}

/// The `agw` Service: the seat ports only (the readiness port is reached by exec, never by a
/// seat).
pub(crate) fn gateway_service(g: &GatewaySpec, ports: &[u16]) -> Value {
    let ports: Vec<Value> = ports
        .iter()
        .map(|p| json!({"name": format!("p{p}"), "protocol": "TCP", "port": p, "targetPort": p}))
        .collect();
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": meta(GATEWAY_NAME, Some(&g.namespace), &g.owner, &[(ROLE_LABEL, "gateway")]),
        "spec": {
            "type": "ClusterIP",
            "selector": {super::OWNER_LABEL: g.owner, ROLE_LABEL: "gateway"},
            "ports": ports,
        }
    })
}

/// The run's sync pod: the only pod that mounts the replica writable as a whole. The engine
/// copies trees in and reads changes back through it, after the seat pods are gone, so no seat
/// process is alive during a write-back (PARITY A2, B11). No network, no gateway, no seat image
/// code: busybox and git from the floor's checks image, run only by the engine.
pub(crate) fn sync_pod(
    ns: &str,
    owner: &str,
    run_label: &str,
    image: &str,
    floor_image: &str,
) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        // The boot label: a restarted engine's sweep reaps a sync pod its predecessor left with a
        // command still running in it (review r2 P2).
        "metadata": meta("sync", Some(ns), owner, &[
            (RUN_LABEL, run_label), (ROLE_LABEL, "sync"), (super::BOOT_LABEL, super::boot_id()),
        ]),
        "spec": {
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 1,
            "securityContext": pod_context(POD_UID),
            "initContainers": [floor_init(floor_image)],
            "containers": [{
                "name": "sync",
                "image": image,
                "imagePullPolicy": "Never",
                "command": IDLE_COMMAND,
                "securityContext": hardened_context(),
                "resources": {
                    "requests": {"memory": "64Mi", "cpu": "20m"},
                    "limits": {"memory": "1Gi"},
                },
                "volumeMounts": [
                    {"name": "floor", "mountPath": FLOOR_DIR, "readOnly": true},
                    {"name": "replica", "mountPath": "/replica"},
                    {"name": "tmp", "mountPath": "/tmp"},
                ],
            }],
            "volumes": [
                {"name": "floor", "emptyDir": {"sizeLimit": "64Mi"}},
                {"name": "replica", "persistentVolumeClaim": {"claimName": REPLICA_PVC}},
                {"name": "tmp", "emptyDir": {"sizeLimit": "1Gi"}},
            ],
        }
    })
}

/// The role of a cache fill pod: the only floor pod with the network ([`fill_egress`]).
pub(crate) const FILL_ROLE: &str = "fill";

/// Everything one checks-floor pod (a floor side, a cache fill or a tool probe) is created with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FloorPod {
    pub(crate) name: String,
    pub(crate) namespace: String,
    pub(crate) owner: String,
    pub(crate) run_label: String,
    /// `floor`, [`FILL_ROLE`] or `probe`.
    pub(crate) role: String,
    /// The checks-floor image.
    pub(crate) image: String,
    /// The seat floor image (the PID 1 loop's busybox).
    pub(crate) floor_image: String,
    pub(crate) memory_limit: String,
    pub(crate) memory_request: String,
    pub(crate) env: Vec<(String, String)>,
    /// Scratch dirs, in memory: (mount path, size limit).
    pub(crate) empty: Vec<(String, String)>,
    /// Claims of the namespace: (claim, mount path, read-only).
    pub(crate) claims: Vec<(String, String, bool)>,
    /// Directories made inside the scratch dirs before the pod's container starts, as the pod's
    /// user: the parents of a claim mounted inside a scratch dir (a nested package's
    /// `node_modules`), which the runtime would otherwise make as root, unwritable to the checks.
    pub(crate) mkdirs: Vec<String>,
    pub(crate) deadline_secs: u64,
}

/// A checks-floor pod (PARITY B13): the checks image idling under the engine's PID 1 loop, no
/// network but what its role's policy grants (none for `floor` and `probe`), restricted, no
/// service-account token, the exit-code dir in memory, and each claim at its path.
pub(crate) fn floor_pod(f: &FloorPod) -> Result<Value, String> {
    let mut mounts = vec![
        json!({"name": "floor", "mountPath": FLOOR_DIR, "readOnly": true}),
        json!({"name": "rc", "mountPath": super::floor::RC_DIR}),
    ];
    let mut volumes = vec![
        json!({"name": "floor", "emptyDir": {"sizeLimit": "64Mi"}}),
        json!({"name": "rc", "emptyDir": {"medium": "Memory", "sizeLimit": "1Mi"}}),
    ];
    let mut paths: Vec<&str> = vec![FLOOR_DIR, super::floor::RC_DIR];
    for (i, (path, size)) in f.empty.iter().enumerate() {
        paths.push(path);
        let name = format!("e{i}");
        mounts.push(json!({"name": name, "mountPath": path}));
        // In memory, like the Docker floor's tmpfs (review r1): charged to the pod's memory cap,
        // so a check cannot fill the node's disk through unlinked or unaccounted files.
        volumes.push(json!({"name": name, "emptyDir": {"medium": "Memory", "sizeLimit": size}}));
    }
    for (i, (claim, path, ro)) in f.claims.iter().enumerate() {
        paths.push(path);
        let name = format!("c{i}");
        mounts.push(json!({"name": name, "mountPath": path, "readOnly": ro}));
        volumes.push(
            json!({"name": name, "persistentVolumeClaim": {"claimName": claim, "readOnly": ro}}),
        );
    }
    // Shallower first: a scratch dir mounted inside a claim (a native addon's `build/` inside a
    // read-only `node_modules`, core#1393) lands over it, never under it.
    mounts.sort_by_key(|m| {
        m["mountPath"]
            .as_str()
            .map(|p| p.split('/').filter(|c| !c.is_empty()).count())
            .unwrap_or(0)
    });
    let mut seen = std::collections::HashSet::new();
    for p in &paths {
        if !p.starts_with('/') || p.split('/').any(|c| c == "..") || !seen.insert(*p) {
            return Err(format!("`{p}` cannot be a floor pod mount"));
        }
    }
    let env: Vec<Value> = f
        .env
        .iter()
        .map(|(k, v)| json!({"name": k, "value": v}))
        .collect();
    let mut init = vec![floor_init(&f.floor_image)];
    if !f.mkdirs.is_empty() {
        for d in &f.mkdirs {
            let inside = f.empty.iter().any(|(p, _)| d.starts_with(&format!("{p}/")));
            if !inside || d.split('/').any(|c| c == "..") {
                return Err(format!("`{d}` is not inside a scratch dir of the pod"));
            }
        }
        let mut cmd = vec![json!("/busybox"), json!("mkdir"), json!("-p")];
        cmd.extend(f.mkdirs.iter().map(|d| json!(d)));
        // Only the top-level scratch dirs (B-3 review r2): a deeper scratch mount (a native
        // addon's `build/`) would make its parents as root before this container runs.
        let tops: Vec<&String> = f
            .empty
            .iter()
            .map(|(p, _)| p)
            .filter(|p| {
                !f.empty
                    .iter()
                    .any(|(q, _)| q != *p && p.starts_with(&format!("{q}/")))
                    && !f
                        .claims
                        .iter()
                        .any(|(_, q, _)| p.starts_with(&format!("{q}/")))
            })
            .collect();
        let scratch: Vec<Value> = mounts
            .iter()
            .filter(|m| m["name"].as_str().is_some_and(|n| n.starts_with('e')))
            .filter(|m| {
                m["mountPath"]
                    .as_str()
                    .is_some_and(|p| tops.iter().any(|t| t.as_str() == p))
            })
            .cloned()
            .collect();
        init.push(json!({
            "name": "mkdirs",
            "image": f.floor_image,
            "imagePullPolicy": "Never",
            "command": cmd,
            "securityContext": hardened_context(),
            "resources": {"requests": {"memory": "16Mi", "cpu": "10m"}, "limits": {"memory": "64Mi"}},
            "volumeMounts": scratch,
        }));
    }
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": meta(&f.name, Some(&f.namespace), &f.owner, &[
            (RUN_LABEL, &f.run_label), (ROLE_LABEL, &f.role), (super::BOOT_LABEL, super::boot_id()),
        ]),
        "spec": {
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            "hostNetwork": false,
            "hostPID": false,
            "hostIPC": false,
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 1,
            "activeDeadlineSeconds": f.deadline_secs,
            "securityContext": pod_context(POD_UID),
            "initContainers": init,
            "containers": [{
                "name": super::floor::CONTAINER,
                "image": f.image,
                "imagePullPolicy": "Never",
                "command": IDLE_COMMAND,
                "env": env,
                "securityContext": hardened_context(),
                "resources": {
                    "requests": {"memory": f.memory_request, "cpu": "100m"},
                    "limits": {"memory": f.memory_limit},
                },
                "volumeMounts": mounts,
            }],
            "volumes": volumes,
        }
    }))
}

/// Every object of a run namespace, in apply order (the namespace first).
/// The annotation holding a run namespace's exact run id (a label value may be sanitized; the
/// sweep asks the engine about the exact id, review r1 P1).
pub(crate) const RUN_ID_ANNOTATION: &str = "wicked.dev/run-id";

pub(crate) fn run_namespace_docs(
    ns: &str,
    owner: &str,
    run_id: &str,
    daemon_ns: &str,
    budget: &RunBudget,
    replica_size: &str,
) -> Vec<Value> {
    let mut ns_doc = namespace(
        ns,
        owner,
        &[
            (RUN_LABEL, &super::label_value(run_id)),
            (ROLE_LABEL, "run"),
        ],
    );
    ns_doc["metadata"]["annotations"] = json!({RUN_ID_ANNOTATION: run_id});
    vec![
        ns_doc,
        resource_quota(ns, owner, budget),
        limit_range(ns, owner),
        default_deny(ns, owner),
        seat_egress(ns, owner, daemon_ns, seat_gateway_ports()),
        fill_egress(ns, owner),
        pvc(ns, owner, REPLICA_PVC, replica_size),
    ]
}

/// The most processes a pod may hold (Docker's `--pids-limit` was 4096, PARITY B2).
pub(crate) const POD_PIDS_MAX: u32 = 4096;
/// How far past [`POD_PIDS_MAX`] the probe forks before it calls the pod unbounded.
pub(crate) const PIDS_PROBE_SLACK: u32 = 64;
/// The line the probe prints when every fork succeeded.
pub(crate) const PIDS_PROBE_UNBOUNDED: &str = "unbounded";

/// The pids-limit probe (PARITY B2, D3): inside a container `pids.max` reads `max` even when the
/// kubelet limits the POD (the limit sits on the pod's parent cgroup, outside the container's
/// cgroup namespace), so the limit is measured by behaviour: fork idle children until a fork is
/// refused, printing the count. No network (the daemon namespace denies it), no seat image, no
/// credentials; the container's exit ends every child.
pub(crate) fn pids_probe_pod(
    ns: &str,
    owner: &str,
    name: &str,
    floor_image: &str,
    node: &str,
) -> Value {
    let limit = POD_PIDS_MAX + PIDS_PROBE_SLACK;
    // First the policy enforcement check (review r1 P1): this pod's namespace denies all egress,
    // so the API server and the node's kubelet must not answer it; then the fork count. A new
    // pod's policy lands after its first instant (PARITY F1), later under load (B-2 live: still
    // open after 3 s at load ~60), so the check polls up to ~20 s for every target to close; a
    // cluster that never enforces still answers open at the end.
    let script = format!(
        "i=0; while :; do r=''; for t in \"$KUBERNETES_SERVICE_HOST 443\" \"$HOST_IP 10250\" \"1.1.1.1 443\"; do \
         if /busybox nc -w 2 $t </dev/null >/dev/null 2>&1; then r=\"$r open\"; else r=\"$r closed\"; fi; done; \
         i=$((i+1)); case \"$r\" in *open*) [ $i -lt 10 ] && {{ /busybox sleep 2; continue; }};; esac; break; done; \
         for n in $r; do echo net=$n; done; \
         i=0; while [ $i -lt {limit} ]; do /busybox sleep 120 & i=$((i+1)); echo $i; done; \
         echo {PIDS_PROBE_UNBOUNDED}"
    );
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": meta(name, Some(ns), owner, &[(ROLE_LABEL, "probe")]),
        "spec": {
            "nodeName": node,
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 0,
            "activeDeadlineSeconds": 120,
            "securityContext": pod_context(POD_UID),
            "containers": [{
                "name": "probe",
                "image": floor_image,
                "imagePullPolicy": "Never",
                "command": ["/busybox", "sh", "-c", script],
                "env": [{"name": "HOST_IP", "valueFrom": {"fieldRef": {"fieldPath": "status.hostIP"}}}],
                "securityContext": hardened_context(),
                "resources": {
                    "requests": {"memory": "64Mi", "cpu": "50m"},
                    "limits": {"memory": "768Mi"},
                },
            }],
        }
    })
}

/// The probe's verdict from its log: `Ok(n)` = a fork was refused after `n` children, within
/// the limit; `Err` = the pod forked past [`POD_PIDS_MAX`] (no limit, or one too high) or the log
/// says nothing usable.
pub(crate) fn pids_probe_verdict(log: &str) -> Result<u32, String> {
    let net: Vec<&str> = log
        .lines()
        .filter_map(|l| l.trim().strip_prefix("net="))
        .collect();
    // No network answer at all is a probe that did not run to its end (evicted, cut): an
    // infrastructure failure to retry, never a claim about the cluster's policy (live, B-2).
    if net.is_empty() {
        return Err(
            "the pids-limit probe printed no network answer (its pod was evicted or its log \
             was cut); retry"
                .into(),
        );
    }
    if net.len() != 3 || net.iter().any(|n| *n != "closed") {
        return Err(format!(
            "the cluster does not enforce NetworkPolicy: a pod in a deny-all namespace reached \
             the API server, the node's kubelet or the internet ({net:?})"
        ));
    }
    if log.lines().any(|l| l.trim() == PIDS_PROBE_UNBOUNDED) {
        return Err(format!(
            "a pod forked {} processes without refusal: the cluster sets no pod pids limit (or \
             one above {POD_PIDS_MAX})",
            POD_PIDS_MAX + PIDS_PROBE_SLACK
        ));
    }
    let last = log
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .next_back()
        .ok_or("the pids probe printed no count")?;
    if !log.contains("can't fork") {
        return Err(format!(
            "the pids probe stopped after {last} children without a refused fork"
        ));
    }
    // The pod also holds the probe's shell and the pod's pause process: a limit of N refuses
    // the child after N - 2. A refusal far below the limit is pressure elsewhere (the node's
    // pid space, memory), not evidence of a pod limit (review r1 P2/P3): never a pass.
    let most = POD_PIDS_MAX - 2;
    if last > most {
        return Err(format!(
            "a pod forked {last} children: the pod pids limit is above {POD_PIDS_MAX}"
        ));
    }
    if last + PIDS_PROBE_SLACK * 2 < most {
        return Err(format!(
            "a fork was refused after only {last} children: not the pod's own limit; retry"
        ));
    }
    Ok(last)
}
