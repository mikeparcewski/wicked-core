//! The Kubernetes runner's pure parity tests (PARITY.md rows named per test). Live tests against
//! a real cluster are gated by `WICKED_K8S_LIVE=1` (see `live`).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::cluster::{self, SweepAction};
use super::render::{self, GatewaySpec, HostPort, ReplicaMount, RunBudget, SeatPod};
use super::*;

fn kube() -> Kube {
    Kube {
        kubectl: PathBuf::from("/opt/k/kubectl"),
        kubeconfig: Some(PathBuf::from("/state/k8s/kubeconfig")),
        context: "wicked".into(),
        owner: "wabc123def456".into(),
    }
}

fn seat() -> SeatPod {
    SeatPod {
        name: "seat-1".into(),
        namespace: "wicked-run-wabc-0011223344".into(),
        owner: "wabc123def456".into(),
        run_label: "run-1".into(),
        unit_label: "u1".into(),
        boot: "b1".into(),
        role: "seat".into(),
        image: "wicked-seat-codex:abc".into(),
        floor_image: "wicked-floor:def".into(),
        workdir: PathBuf::from("/home/op/.wicked/repos/x/wt/run-1"),
        mounts: vec![
            ReplicaMount {
                path: PathBuf::from("/home/op/.wicked/repos/x/wt/run-1"),
                rw: true,
            },
            ReplicaMount {
                path: PathBuf::from("/home/op/.wicked/skills/snap"),
                rw: false,
            },
        ],
        memory_limit: "3Gi".into(),
        memory_request: "768Mi".into(),
        cpu_request: "100m".into(),
        gateway_ip: Some("10.43.0.50".into()),
        shutdown_time: "2026-11-11T05:00:00Z".into(),
        deadline_secs: 3600,
        scope: None,
        view: None,
    }
}

fn gw() -> GatewaySpec {
    GatewaySpec {
        namespace: "wicked-sys-wabc123def456".into(),
        owner: "wabc123def456".into(),
        image: crate::seat_gateway::GATEWAY_IMAGE.into(),
        writer_image: "busybox:1.37".into(),
        boot: "boot-1".into(),
    }
}

/// R7 / D5: every kubectl call names its kubeconfig, its context and (when namespaced) its
/// namespace, and the inherited `KUBECONFIG` is removed.
#[test]
fn k8s_every_kubectl_argv_pins_the_kubeconfig_context_and_namespace() {
    let k = kube();
    assert_eq!(
        k.head(Some("ns1")),
        [
            "/opt/k/kubectl",
            "--kubeconfig",
            "/state/k8s/kubeconfig",
            "--context",
            "wicked",
            "--namespace",
            "ns1"
        ]
    );
    assert_eq!(k.head(None).len(), 5);
    let cmd = k.cmd(None);
    assert!(cmd
        .get_envs()
        .any(|(key, v)| key == "KUBECONFIG" && v.is_none()));
}

/// D5: the current kubectl context is never used implicitly.
#[test]
fn k8s_no_configured_cluster_is_no_cluster_never_the_current_context() {
    let dir = std::env::temp_dir().join(format!("wicked-k8s-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let home = Gone(dir);
    let home = &home;
    let on_path = |_: &str| Some(PathBuf::from("/usr/bin/kubectl"));
    let err = resolve_settings(home.0.as_path(), None, None, None, None, on_path).unwrap_err();
    assert_eq!(err.0, FloorUnarmed::NoCluster);
    // An explicit context is honoured, on kubectl's own kubeconfig.
    let k = resolve_settings(
        home.0.as_path(),
        None,
        None,
        Some("rancher-desktop"),
        None,
        on_path,
    )
    .unwrap();
    assert_eq!(k.context, "rancher-desktop");
    assert_eq!(k.kubeconfig, None);
    // The engine's own kubeconfig, once installed, is the default.
    let own = own_kubeconfig(home.0.as_path());
    std::fs::create_dir_all(own.parent().unwrap()).unwrap();
    std::fs::write(&own, "{}").unwrap();
    let k = resolve_settings(home.0.as_path(), None, None, None, None, on_path).unwrap();
    assert_eq!(k.kubeconfig.as_deref(), Some(own.as_path()));
    assert_eq!(k.context, OWN_CONTEXT);
    // No kubectl at all (an explicit choice that does not exist is the only candidate).
    let err = resolve_settings(
        home.0.as_path(),
        Some("/nonexistent/kubectl"),
        None,
        None,
        None,
        |_| None,
    )
    .unwrap_err();
    assert_eq!(err.0, FloorUnarmed::NoCluster);
}

#[test]
fn k8s_names_are_dns_labels_and_label_values_never_collide() {
    let owner = owner_id(Path::new("/home/op/.wicked"));
    assert!(owner.len() == 13 && owner.starts_with('w'));
    assert_ne!(owner, owner_id(Path::new("/home/op/.wicked-dev")));
    let ns = run_namespace_name(&owner, "9dda9f62-1234-4abc-8def-0123456789ab");
    assert!(ns.len() <= 63, "{ns}");
    assert!(ns
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    assert_eq!(label_value("run-1"), "run-1");
    let a = label_value("a b");
    let b = label_value("a/b");
    assert_ne!(a, b);
    assert!(label_value(&"x".repeat(100)).len() <= 63);
    assert!(label_value("--").starts_with('x'));
    // Review r2 P3: a label is a selector, not an identity: this pair maps to one label, which
    // is why the sweep matches runs by their exact-id annotation.
    assert_eq!(label_value("a b"), label_value(&label_value("a b")));
}

/// B1: every engine namespace enforces PodSecurity `restricted`.
#[test]
fn k8s_every_engine_namespace_enforces_restricted() {
    let docs = render::run_namespace_docs("ns", "o", "r", "d", &RunBudget::default(), "20Gi");
    let ns = &docs[0];
    assert_eq!(ns["kind"], "Namespace");
    let l = &ns["metadata"]["labels"];
    assert_eq!(l["pod-security.kubernetes.io/enforce"], "restricted");
    assert_eq!(l[OWNER_LABEL], "o");
    assert_eq!(l[ROLE_LABEL], "run");
    let install = cluster::install_docs("o");
    assert_eq!(
        install[0]["metadata"]["labels"]["pod-security.kubernetes.io/enforce"],
        "restricted"
    );
}

/// B1, B3, F1: the seat pod is restricted and hardened, idles under the engine's init and
/// starts no workload at container start.
#[test]
fn k8s_the_seat_pod_spec_is_restricted_and_hardened() {
    let spec = render::seat_pod_spec(&seat()).unwrap();
    assert_eq!(spec["automountServiceAccountToken"], false);
    assert_eq!(spec["enableServiceLinks"], false);
    for k in ["hostNetwork", "hostPID", "hostIPC"] {
        assert_eq!(spec[k], false, "{k}");
    }
    let psc = &spec["securityContext"];
    assert_eq!(psc["runAsNonRoot"], true);
    assert_eq!(psc["seccompProfile"]["type"], "RuntimeDefault");
    let c = &spec["containers"][0];
    let sc = &c["securityContext"];
    assert_eq!(sc["allowPrivilegeEscalation"], false);
    assert_eq!(sc["readOnlyRootFilesystem"], true);
    assert_eq!(sc["capabilities"]["drop"], json!(["ALL"]));
    assert_eq!(c["imagePullPolicy"], "Never");
    // PID 1 is the engine's idle loop from the trusted floor files: never the CLI (the policy
    // lands after start, F1), never the seat image's own entrypoint.
    assert_eq!(c["command"], json!(render::IDLE_COMMAND));
    assert_eq!(c["command"][0], "/floor/busybox");
    assert!(!render::IDLE_COMMAND.join(" ").contains("codex"));
    assert_eq!(c["resources"]["limits"]["memory"], "3Gi");
    assert_eq!(c["resources"]["requests"]["memory"], "768Mi");
    // The floor files come from the engine's image, read-only.
    assert_eq!(spec["initContainers"][0]["image"], "wicked-floor:def");
    let mounts = c["volumeMounts"].as_array().unwrap();
    let floor = mounts.iter().find(|m| m["mountPath"] == "/floor").unwrap();
    assert_eq!(floor["readOnly"], true);
    assert_eq!(spec["activeDeadlineSeconds"], 3600);
    // The canary and unit dirs, /tmp and HOME are in memory (bounded by the memory limit, like
    // the container floor's tmpfs; review r1 P2).
    let vols = spec["volumes"].as_array().unwrap();
    for name in ["canary", "unit", "tmp", "home"] {
        let v = vols.iter().find(|v| v["name"] == name).unwrap();
        assert_eq!(v["emptyDir"]["medium"], "Memory", "{name}");
    }
    // No host path of any kind.
    assert!(!spec.to_string().contains("hostPath"));
    assert_eq!(spec["hostAliases"][0]["hostnames"], json!(["agw"]));
}

/// B6, B11: each replica path is mounted at its host path with the mount plan's access; a
/// path that could cover a reserved pod path, or is not absolute and normal, is refused.
#[test]
fn k8s_replica_paths_mount_at_their_host_paths_with_their_access() {
    let spec = render::seat_pod_spec(&seat()).unwrap();
    let mounts = spec["containers"][0]["volumeMounts"].as_array().unwrap();
    let tree = mounts
        .iter()
        .find(|m| m["mountPath"] == "/home/op/.wicked/repos/x/wt/run-1")
        .unwrap();
    assert_eq!(tree["name"], render::REPLICA_PVC);
    assert_eq!(tree["subPath"], "home/op/.wicked/repos/x/wt/run-1");
    assert_eq!(tree["readOnly"], false);
    let skills = mounts
        .iter()
        .find(|m| m["mountPath"] == "/home/op/.wicked/skills/snap")
        .unwrap();
    assert_eq!(skills["readOnly"], true);
    // A path under `/tmp` is allowed (an engine session's scratch tree, review r1 of B-2).
    for bad in [
        "/tmp",
        "/floor",
        "/floor/x",
        "/wicked-canary",
        "/",
        "/a/../b",
        "rel/x",
    ] {
        let mut s = seat();
        s.mounts.push(ReplicaMount {
            path: PathBuf::from(bad),
            rw: false,
        });
        assert!(render::seat_pod_spec(&s).is_err(), "{bad}");
    }
}

/// C1: a seat's Sandbox is never re-created and is deleted by the controller at its deadline,
/// even with no engine left to reap it.
#[test]
fn k8s_a_seat_sandbox_has_a_dead_mans_switch_and_is_never_recreated() {
    let sb = render::seat_sandbox(&seat()).unwrap();
    assert_eq!(sb["kind"], "Sandbox");
    assert_eq!(sb["apiVersion"], "agents.x-k8s.io/v1beta1");
    assert_eq!(sb["spec"]["shutdownPolicy"], "Delete");
    assert_eq!(sb["spec"]["shutdownTime"], "2026-11-11T05:00:00Z");
    assert_eq!(sb["spec"]["failedPodPolicy"], "Retain");
    assert_eq!(sb["spec"]["service"], false);
    let l = &sb["spec"]["podTemplate"]["metadata"]["labels"];
    assert_eq!(l[OWNER_LABEL], "wabc123def456");
    assert_eq!(l[UNIT_LABEL], "u1");
    assert_eq!(l[BOOT_LABEL], "b1");
    assert_eq!(l[ROLE_LABEL], "seat");
}

/// B5: the run namespace denies everything; seats and judges may reach only the daemon's
/// gateway pods, only on the seat ports, with no DNS; nothing else in the run gets egress.
#[test]
fn k8s_the_run_network_policy_allows_only_gateway_ports() {
    let docs = render::run_namespace_docs("ns", "o", "r", "dns1", &RunBudget::default(), "20Gi");
    let deny = docs
        .iter()
        .find(|d| d["metadata"]["name"] == "default-deny")
        .unwrap();
    assert_eq!(deny["spec"]["podSelector"], json!({}));
    assert_eq!(deny["spec"]["policyTypes"], json!(["Ingress", "Egress"]));
    let policies: Vec<&Value> = docs
        .iter()
        .filter(|d| d["kind"] == "NetworkPolicy")
        .collect();
    assert_eq!(policies.len(), 2, "default-deny plus the seat egress only");
    let egress = policies
        .iter()
        .find(|d| d["metadata"]["name"] == "seat-egress-gateway-only")
        .unwrap();
    let sel = &egress["spec"]["podSelector"]["matchExpressions"][0];
    assert_eq!(sel["values"], json!(["seat", "judge"]));
    let rules = egress["spec"]["egress"].as_array().unwrap();
    assert_eq!(rules.len(), 1);
    let to = &rules[0]["to"][0];
    assert_eq!(
        to["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
        "dns1"
    );
    assert_eq!(to["podSelector"]["matchLabels"][ROLE_LABEL], "gateway");
    assert!(to.get("ipBlock").is_none());
    let ports: Vec<u64> = rules[0]["ports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["port"].as_u64().unwrap())
        .collect();
    let want: Vec<u64> = crate::container_floor::GATEWAY_PORTS
        .iter()
        .map(|p| u64::from(*p))
        .collect();
    assert_eq!(ports, want);
    assert!(!ports.contains(&53));
    assert!(!ports.contains(&u64::from(crate::seat_gateway::READY_PORT)));
}

/// B8, B9: the broker is read-only in the gateway container and writable only in its writer
/// sidecar; the Service exposes the seat ports and never the readiness port; the gateway admits
/// only this daemon's run namespaces' seats and judges.
#[test]
fn k8s_the_broker_is_writable_only_in_the_writer_and_the_service_hides_readiness() {
    let d = render::gateway_deployment(&gw());
    let cs: Vec<Value> = d["spec"]["template"]["spec"]["containers"]
        .as_array()
        .unwrap()
        .iter()
        .chain(
            d["spec"]["template"]["spec"]["initContainers"]
                .as_array()
                .unwrap(),
        )
        .cloned()
        .collect();
    let mount = |c: &Value, path: &str| -> Value {
        c["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["mountPath"] == path)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let gateway = cs.iter().find(|c| c["name"] == "gateway").unwrap();
    let writer = cs.iter().find(|c| c["name"] == "writer").unwrap();
    assert_eq!(
        gateway["readinessProbe"]["tcpSocket"]["port"],
        crate::seat_gateway::READY_PORT
    );
    assert_eq!(mount(gateway, "/broker")["readOnly"], true);
    assert_eq!(mount(gateway, "/cfg")["readOnly"], true);
    assert!(mount(writer, "/broker").get("readOnly").is_none());
    assert_eq!(
        d["spec"]["template"]["spec"]["automountServiceAccountToken"],
        false
    );
    assert_eq!(
        writer["restartPolicy"], "Always",
        "the writer is a native sidecar"
    );
    for c in &cs {
        assert_eq!(c["securityContext"]["readOnlyRootFilesystem"], true);
    }
    let svc = render::gateway_service(&gw(), render::seat_gateway_ports());
    let ports: Vec<u64> = svc["spec"]["ports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["port"].as_u64().unwrap())
        .collect();
    assert!(!ports.contains(&u64::from(crate::seat_gateway::READY_PORT)));
    let ing = render::gateway_ingress("dns", "o1", render::seat_gateway_ports());
    let from = &ing["spec"]["ingress"][0]["from"][0];
    assert_eq!(from["namespaceSelector"]["matchLabels"][OWNER_LABEL], "o1");
    assert_eq!(from["namespaceSelector"]["matchLabels"][ROLE_LABEL], "run");
}

/// B10: the gateway's own egress is the internet and DNS, never a private or cluster range,
/// plus exactly the host ports it is given.
#[test]
fn k8s_the_gateway_reaches_the_internet_and_named_host_ports_only() {
    let e = render::gateway_egress(
        "dns",
        "o",
        &[HostPort {
            ip: "192.168.5.2".into(),
            port: 8711,
        }],
    );
    let rules = e["spec"]["egress"].as_array().unwrap();
    let internet = &rules[0]["to"][0]["ipBlock"];
    assert_eq!(internet["cidr"], "0.0.0.0/0");
    for r in render::PRIVATE_RANGES {
        assert!(internet["except"].as_array().unwrap().contains(&json!(r)));
    }
    let host = rules.last().unwrap();
    assert_eq!(host["to"][0]["ipBlock"]["cidr"], "192.168.5.2/32");
    assert_eq!(host["ports"][0]["port"], 8711);
}

/// D5 (review r1 P1): the engine's account may write only objects carrying its owner label,
/// change or delete only objects that already carried it (no relabel takeover), and act only
/// inside namespaces carrying it; exec and logs are bound per owned namespace, never cluster-wide;
/// it holds no Secret, PersistentVolume or wildcard verb; the policy is applied before the grant.
#[test]
fn k8s_the_engine_account_cannot_change_what_it_does_not_own() {
    let docs = cluster::install_docs("o1");
    let idx = |kind: &str| docs.iter().position(|d| d["kind"] == kind).unwrap();
    assert!(idx("ValidatingAdmissionPolicy") < idx("ClusterRoleBinding"));
    assert!(idx("ValidatingAdmissionPolicyBinding") < idx("ClusterRoleBinding"));
    let vap = &docs[idx("ValidatingAdmissionPolicy")];
    let cond = vap["spec"]["matchConditions"][0]["expression"]
        .as_str()
        .unwrap();
    assert!(cond.contains("system:serviceaccount:wicked-sys-o1:wicked-engine"));
    assert_eq!(vap["spec"]["failurePolicy"], "Fail");
    let ops = &vap["spec"]["matchConstraints"]["resourceRules"][0]["operations"];
    assert_eq!(ops, &json!(["CREATE", "UPDATE", "DELETE"]));
    let rules: Vec<String> = vap["spec"]["validations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["expression"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(rules.len(), 3);
    assert!(rules[0].starts_with("request.operation == 'DELETE' || (object != null"));
    assert!(rules[1].starts_with("request.operation == 'CREATE' || (oldObject != null"));
    assert!(rules[2].contains("namespaceObject != null"));
    for r in &rules {
        assert!(r.contains("['wicked.dev/owner'] == 'o1'"), "{r}");
        assert!(!r.contains("|| true"));
    }
    let binding = &docs[idx("ValidatingAdmissionPolicyBinding")];
    assert_eq!(binding["spec"]["validationActions"], json!(["Deny"]));
    let main = docs
        .iter()
        .find(|d| d["kind"] == "ClusterRole" && d["metadata"]["name"] == "wicked-engine-o1")
        .unwrap();
    let text = main["rules"].to_string();
    for banned in [
        "\"secrets\"",
        "\"*\"",
        "persistentvolumes\"",
        "pods/exec",
        "pods/log",
        "escalate",
        "impersonate",
    ] {
        assert!(!text.contains(banned), "{banned} in the cluster-wide role");
    }
    // exec/logs: a ClusterRole bound only by RoleBindings in owned namespaces.
    assert!(!docs.iter().any(|d| d["kind"] == "ClusterRoleBinding"
        && d["roleRef"]["name"] == cluster::exec_role_name("o1")));
    let rb = cluster::exec_role_binding("wicked-run-o1-x", "o1");
    assert_eq!(rb["kind"], "RoleBinding");
    assert_eq!(rb["metadata"]["namespace"], "wicked-run-o1-x");
    assert_eq!(rb["metadata"]["labels"][OWNER_LABEL], "o1");
    // No long-lived token Secret in the cluster's datastore.
    assert!(!docs.iter().any(|d| d["kind"] == "Secret"));
}

/// Review r1 P2: a delete is bound to the exact object the ownership check read.
#[test]
fn k8s_a_namespace_delete_is_bound_to_the_object_that_was_checked() {
    let obj = json!({"metadata": {"uid": "u-1", "resourceVersion": "42"}});
    let d = cluster::delete_preconditions(&obj).unwrap();
    assert_eq!(d["preconditions"]["uid"], "u-1");
    assert_eq!(d["preconditions"]["resourceVersion"], "42");
    assert!(cluster::delete_preconditions(&json!({"metadata": {}})).is_err());
}

/// Review r1 P2: the engine's credential is a TokenRequest token, renewed from its expiry.
#[test]
fn k8s_the_engine_token_expiry_is_read_for_renewal() {
    use base64::Engine as _;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1900000000}"#);
    assert_eq!(
        cluster::token_expiry(&format!("h.{payload}.s")),
        Some(1_900_000_000)
    );
    assert_eq!(cluster::token_expiry("not-a-jwt"), None);
}

/// D5: the controller is the pinned release, byte for byte and by image.
#[test]
fn k8s_the_controller_is_the_pinned_release() {
    assert!(cluster::check_manifest(cluster::AGENT_SANDBOX_MANIFEST).is_ok());
    assert!(cluster::check_manifest("kind: Namespace").is_err());
    let dep = |image: &str, observed: u64, updated: u64, avail: u64| {
        json!({"metadata": {"generation": 2},
               "spec": {"replicas": 1, "template": {"spec": {"containers": [{"image": image}]}}},
               "status": {"observedGeneration": observed, "updatedReplicas": updated,
                          "availableReplicas": avail, "replicas": updated.max(1)}})
    };
    // Review r2 P2: the old controller still available beside an unready new one.
    let mut mixed = dep(AGENT_SANDBOX_IMAGE, 2, 1, 1);
    mixed["status"]["replicas"] = json!(2);
    assert!(cluster::judge_controller(&mixed).is_err());
    assert!(cluster::judge_controller(&dep(AGENT_SANDBOX_IMAGE, 2, 1, 1)).is_ok());
    assert!(cluster::judge_controller(&dep(AGENT_SANDBOX_IMAGE, 2, 1, 0)).is_err());
    assert!(cluster::judge_controller(&dep("registry.k8s.io/x:v1.0.9", 2, 1, 1)).is_err());
    // Review r1 P2: an old ReplicaSet's available pods under the new template are not the pin.
    assert!(cluster::judge_controller(&dep(AGENT_SANDBOX_IMAGE, 1, 0, 1)).is_err());
    assert!(cluster::judge_controller(&dep(AGENT_SANDBOX_IMAGE, 2, 0, 1)).is_err());
}

/// C1: the sweep only ever acts on this daemon's run namespaces; a live run keeps its
/// namespace and loses only an earlier engine's pods.
#[test]
fn k8s_sweep_touches_only_this_daemons_labelled_namespaces() {
    let ns = |name: &str, owner: &str, role: &str, run: &str| {
        json!({"metadata": {"name": name,
                            "labels": {OWNER_LABEL: owner, ROLE_LABEL: role, RUN_LABEL: label_value(run)},
                            "annotations": {(render::RUN_ID_ANNOTATION): run}}})
    };
    // Review r1 P1: an id the label had to sanitize is still matched to its live run.
    let long_live = format!("r:live/{}", "x".repeat(80));
    let all = vec![
        ns("a", "me", "run", &long_live),
        ns("b", "me", "run", "r-done"),
        ns("c", "other", "run", "r-done"),
        ns("d", "me", "daemon", ""),
        json!({"metadata": {"name": "e", "labels": {OWNER_LABEL: "me", ROLE_LABEL: "run"}}}),
        json!({"metadata": {"name": "kube-system"}}),
    ];
    let plan = cluster::sweep_plan(&all, "me", &|r| r == long_live);
    assert_eq!(
        plan,
        vec![
            SweepAction::ReapStalePods("a".into()),
            SweepAction::DeleteNamespace("b".into()),
        ],
        "a run namespace with no exact run id is never deleted"
    );
    let docs =
        render::run_namespace_docs("ns", "me", &long_live, "d", &RunBudget::default(), "1Gi");
    assert_eq!(
        docs[0]["metadata"]["annotations"][render::RUN_ID_ANNOTATION],
        long_live.as_str()
    );
}

#[test]
fn k8s_only_a_ready_undeleted_pod_is_running() {
    let pod = |name: &str, phase: &str, ready: bool, deleting: bool| {
        let cond = if ready { "True" } else { "False" };
        let mut p = json!({"metadata": {"name": name},
                           "status": {"phase": phase, "containerStatuses": [{"ready": ready}],
                                      "conditions": [{"type": "Ready", "status": cond}]}});
        if deleting {
            p["metadata"]["deletionTimestamp"] = json!("2026-11-11T00:00:00Z");
        }
        p
    };
    assert_eq!(
        cluster::running_pod(&[pod("a", "Pending", false, false)]),
        None
    );
    assert_eq!(
        cluster::running_pod(&[pod("a", "Running", false, false)]),
        None
    );
    assert_eq!(
        cluster::running_pod(&[pod("a", "Running", true, true)]),
        None
    );
    assert_eq!(
        cluster::running_pod(&[
            pod("a", "Running", true, true),
            pod("b", "Running", true, false)
        ]),
        Some("b".into())
    );
}

#[test]
fn k8s_a_delete_is_refused_for_an_object_this_daemon_does_not_own() {
    let mine = json!({"metadata": {"labels": {OWNER_LABEL: "me"}}});
    let theirs = json!({"metadata": {"labels": {OWNER_LABEL: "you"}}});
    let none = json!({"metadata": {}});
    assert!(is_owned_by(&mine, "me"));
    assert!(!is_owned_by(&theirs, "me"));
    assert!(!is_owned_by(&none, "me"));
}

#[test]
fn k8s_the_engine_kubeconfig_names_only_its_own_context() {
    // Review r1 P2: the admin context's whole (flattened) cluster entry is kept.
    let cluster_entry = json!({"server": "https://127.0.0.1:6443", "certificate-authority-data": "Q0E=",
                               "tls-server-name": "k8s.local", "proxy-url": "http://p:3128"});
    let cfg = cluster::engine_kubeconfig(cluster_entry.clone(), "tok");
    assert_eq!(cfg["current-context"], OWN_CONTEXT);
    assert_eq!(cfg["contexts"].as_array().unwrap().len(), 1);
    assert_eq!(cfg["users"][0]["user"]["token"], "tok");
    assert_eq!(cfg["clusters"][0]["cluster"], cluster_entry);
}

#[cfg(unix)]
#[test]
fn k8s_a_bounded_call_feeds_stdin_and_reports_the_exit() {
    let k = Kube {
        kubectl: PathBuf::from("/bin/sh"),
        kubeconfig: None,
        context: "x".into(),
        owner: "o".into(),
    };
    // `sh --context x -c ...` is not runnable; use run_bounded directly.
    use wicked_apps_core::HardenedCommand;
    let mut c = std::process::Command::new("/bin/sh");
    c.hardened().args(["-c", "cat; exit 3"]);
    let r = run_bounded(c, Some(b"hello"), std::time::Duration::from_secs(20));
    assert_eq!(r.code, Some(3));
    assert_eq!(r.stdout, b"hello");
    let mut slow = std::process::Command::new("/bin/sh");
    slow.hardened().args(["-c", "sleep 30"]);
    let r = run_bounded(slow, None, std::time::Duration::from_millis(300));
    assert_eq!(r.code, None);
    // Review r2 P2: a descendant holding stdin cannot hold the call past its deadline.
    let mut held = std::process::Command::new("/bin/sh");
    held.hardened().args(["-c", "sleep 30 <&0 & exit 0"]);
    let big = vec![b'x'; 4 * 1024 * 1024];
    let t = std::time::Instant::now();
    let _ = run_bounded(held, Some(&big), std::time::Duration::from_millis(300));
    assert!(
        t.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(k.owner, "o");
}

/// B2 (D3): the pod pids limit is measured by behaviour, never read from the container's own
/// `pids.max` (which reads `max` under the cgroup namespace even when the pod is limited).
#[test]
fn k8s_a_pod_without_a_pids_limit_is_refused() {
    let net = "net=closed\nnet=closed\nnet=closed\n";
    let fork = "sh: can't fork: Resource temporarily unavailable\n";
    let counts = |n: u32| -> String { (1..=n).map(|i| format!("{i}\n")).collect() };
    // The cluster's real answer at --pod-max-pids=4096 (lane data/pids-probe.txt).
    assert_eq!(
        render::pids_probe_verdict(&format!("{net}{}{fork}", counts(4094))),
        Ok(4094)
    );
    let unbounded = format!("{net}{}unbounded\n", counts(4160));
    assert!(render::pids_probe_verdict(&unbounded).is_err());
    // Review r1 P3: a limit above 4096 (4095 children = a 4097 cap) is not parity.
    assert!(render::pids_probe_verdict(&format!("{net}{}{fork}", counts(4095))).is_err());
    // Review r1 P2: a refusal far below the limit is pressure elsewhere, never a pass.
    assert!(render::pids_probe_verdict(&format!("{net}{}{fork}", counts(1200))).is_err());
    // A probe that died for another reason proves nothing.
    assert!(render::pids_probe_verdict(&format!("{net}1\n2\n3\n")).is_err());
    assert!(render::pids_probe_verdict("").is_err());
    // Review r1 P1: a cluster that lets a deny-all pod reach the API server or the kubelet
    // An empty log (an evicted probe) is a retry, never a policy verdict.
    let e = render::pids_probe_verdict("").unwrap_err();
    assert!(
        e.contains("retry") && !e.contains("does not enforce"),
        "{e}"
    );
    // does not enforce NetworkPolicy: refused, whatever the pids answer.
    let open = format!("net=closed\nnet=closed\nnet=open\n{}{fork}", counts(4094));
    assert!(render::pids_probe_verdict(&open)
        .unwrap_err()
        .contains("NetworkPolicy"));
    assert!(render::pids_probe_verdict(&format!("{}{fork}", counts(4094))).is_err());
    let pod = render::pids_probe_pod("ns", "o", "p", "wicked-floor:x", "node-1");
    assert_eq!(pod["spec"]["nodeName"], "node-1");
    assert_eq!(pod["spec"]["automountServiceAccountToken"], false);
    assert_eq!(
        pod["spec"]["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
        true
    );
    let script = pod["spec"]["containers"][0]["command"][3].as_str().unwrap();
    assert!(script.contains("-lt 4160"));
    assert!(script.contains("$KUBERNETES_SERVICE_HOST 443") && script.contains("$HOST_IP 10250"));
    assert!(script.contains("1.1.1.1 443") && script.starts_with("/busybox sleep 3;"));
}

#[test]
fn k8s_shutdown_times_are_rfc3339_utc() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(1_762_819_200), "2025-11-11T00:00:00Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
}
