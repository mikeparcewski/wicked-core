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

/// Seats' and judges' only egress: the gateway pods of the daemon namespace, on the seat ports.
/// No DNS: a seat reaches the gateway by `hostAliases` (closing DNS as a tunnel).
pub(crate) fn seat_egress(ns: &str, owner: &str, daemon_ns: &str, ports: &[u16]) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": meta("seat-egress-gateway-only", Some(ns), owner, &[]),
        "spec": {
            "podSelector": {"matchExpressions": [{
                "key": ROLE_LABEL, "operator": "In", "values": GATEWAY_CLIENT_ROLES,
            }]},
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
                    "podSelector": {"matchExpressions": [{
                        "key": ROLE_LABEL, "operator": "In", "values": GATEWAY_CLIENT_ROLES,
                    }]},
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
    let reserved = [FLOOR_DIR, "/tmp", "/home/seat", "/wicked-canary", UNIT_DIR];
    for m in &s.mounts {
        let sub = replica_sub_path(&m.path)
            .ok_or_else(|| format!("{} cannot be a replica path", m.path.display()))?;
        if reserved.iter().any(|r| {
            m.path == Path::new(r) || m.path.starts_with(r) || Path::new(r).starts_with(&m.path)
        }) {
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
        "securityContext": pod_context(POD_UID),
        "initContainers": [floor_init(&s.floor_image)],
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
            {"name": "tmp", "emptyDir": {"sizeLimit": "2Gi"}},
            {"name": "home", "emptyDir": {"sizeLimit": "512Mi"}},
            {"name": "canary", "emptyDir": {"medium": "Memory", "sizeLimit": "1Mi"}},
            {"name": "unit", "emptyDir": {"medium": "Memory", "sizeLimit": "1Mi"}},
            {"name": "replica", "persistentVolumeClaim": {"claimName": REPLICA_PVC}},
        ],
    });
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
                            "command": ["sh", "-c", "until [ -s /cfg/config.json ]; do sleep 0.2; done"],
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
        "metadata": meta("sync", Some(ns), owner, &[(RUN_LABEL, run_label), (ROLE_LABEL, "sync")]),
        "spec": {
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            "restartPolicy": "Always",
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

/// Every object of a run namespace, in apply order (the namespace first).
pub(crate) fn run_namespace_docs(
    ns: &str,
    owner: &str,
    run_label: &str,
    daemon_ns: &str,
    budget: &RunBudget,
    replica_size: &str,
) -> Vec<Value> {
    vec![
        namespace(ns, owner, &[(RUN_LABEL, run_label), (ROLE_LABEL, "run")]),
        resource_quota(ns, owner, budget),
        limit_range(ns, owner),
        default_deny(ns, owner),
        seat_egress(ns, owner, daemon_ns, seat_gateway_ports()),
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
pub(crate) fn pids_probe_pod(ns: &str, owner: &str, name: &str, floor_image: &str) -> Value {
    let limit = POD_PIDS_MAX + PIDS_PROBE_SLACK;
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": meta(name, Some(ns), owner, &[(ROLE_LABEL, "probe")]),
        "spec": {
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 0,
            "securityContext": pod_context(POD_UID),
            "containers": [{
                "name": "probe",
                "image": floor_image,
                "imagePullPolicy": "Never",
                "command": ["/busybox", "sh", "-c", format!(
                    "i=0; while [ $i -lt {limit} ]; do /busybox sleep 120 & i=$((i+1)); echo $i; done; \
                     echo {PIDS_PROBE_UNBOUNDED}"
                )],
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
    if last > POD_PIDS_MAX {
        return Err(format!(
            "a pod forked {last} processes: the pod pids limit is above {POD_PIDS_MAX}"
        ));
    }
    Ok(last)
}
