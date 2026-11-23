//! Live parity tests against a real cluster. Skipped unless `WICKED_K8S_LIVE=1` and
//! `WICKED_K8S_ADMIN_CONTEXT` names an admin context (used only for `cluster install` and
//! `uninstall`; everything between runs on the engine's own scoped account). The seat image is
//! `WICKED_K8S_LIVE_SEAT_IMAGE` (an image already in the cluster's store). Every object is
//! labelled with a throwaway owner and removed at the end; the run ends with the cluster's
//! labelled-object list as it found it.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use wicked_apps_core::HardenedCommand as _;

use super::cluster;
use super::render::{self, GatewaySpec, ReplicaMount, RunBudget, SeatPod};
use super::*;

fn live() -> Option<(String, String)> {
    if std::env::var("WICKED_K8S_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set WICKED_K8S_LIVE=1 and WICKED_K8S_ADMIN_CONTEXT to run");
        return None;
    }
    // Review r1 P2: once live testing is asked for, a missing context fails, never skips.
    let ctx = std::env::var("WICKED_K8S_ADMIN_CONTEXT")
        .expect("WICKED_K8S_LIVE=1 needs WICKED_K8S_ADMIN_CONTEXT");
    let image = std::env::var("WICKED_K8S_LIVE_SEAT_IMAGE")
        .unwrap_or_else(|_| "wicked-seat-codex:2d518ac11d63".into());
    Some((ctx, image))
}

struct Uninstall(Kube);
impl Drop for Uninstall {
    fn drop(&mut self) {
        match cluster::uninstall(&self.0, false) {
            Ok(gone) => eprintln!("uninstalled: {gone:?}"),
            Err(e) => eprintln!("UNINSTALL FAILED: {e}"),
        }
    }
}

fn exec(
    k: &Kube,
    ns: &str,
    pod: &str,
    container: &str,
    script: &str,
    stdin: Option<&[u8]>,
) -> Called {
    let mut args = vec!["exec"];
    if stdin.is_some() {
        args.push("-i");
    }
    args.extend([
        "-c",
        container,
        pod,
        "--",
        "/floor/busybox",
        "sh",
        "-c",
        script,
    ]);
    k.call(Some(ns), &args, stdin, Duration::from_secs(60))
}

/// B1, B2, B3, B4, B5, B9 (pod layer), C1, D5 end to end on a real cluster: install, the
/// engine's scoped account, the gateway with its writer sidecar, the pids probe, a run
/// namespace, a seat Sandbox (restricted, the PID 1 loop, Landlock canary), the network policy from the
/// seat's side, the dead-man's switch fields, the run's deletion and the uninstall.
#[test]
fn k8s_live_the_cluster_round_trip_holds_every_pod_layer_row() {
    let Some((admin_ctx, seat_image)) = live() else {
        return;
    };
    let t0 = std::time::Instant::now();
    let home =
        std::env::temp_dir().join(format!("wicked-k8s-live-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&home).unwrap();
    let mut admin = resolve_settings(
        &home,
        None,
        None,
        Some(&admin_ctx),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        crate::validator::find_on_path,
    )
    .expect("kubectl and the admin context");
    admin.owner = owner_id(&home);
    let _cleanup = Uninstall(admin.clone());

    // The trusted launcher image, built by the image builder (D2).
    let rt = crate::container_floor::runtime().expect("the image builder answers");
    let floor = crate::container_floor::ensure_floor_image(&rt).expect("the floor image");
    eprintln!("[{:>5.1}s] floor image {floor}", t0.elapsed().as_secs_f64());

    // D5: install, then everything else on the engine's own kubeconfig.
    let kc = cluster::install(&admin, &home).expect("cluster install");
    let engine = resolve_settings(
        &home,
        None,
        None,
        None,
        None,
        crate::validator::find_on_path,
    )
    .expect("the engine's own kubeconfig");
    assert_eq!(engine.kubeconfig.as_deref(), Some(kc.as_path()));
    assert_eq!(engine.owner, admin.owner);
    cluster::probe(&engine).expect("the cluster and controller answer the engine");
    // Review r2: the engine renews its own token as itself (TokenRequest on its own account).
    assert!(cluster::renew_token_below(&engine, u64::MAX).expect("self-renewal"));
    cluster::probe(&engine).expect("the renewed token works");
    eprintln!(
        "[{:>5.1}s] installed; engine probe ok",
        t0.elapsed().as_secs_f64()
    );

    // D5: the engine's account cannot touch an object without its owner label.
    let foreign = serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "not-ours", "namespace": daemon_namespace_name(&engine.owner)}});
    let refused = engine.apply(&[foreign]).unwrap_err();
    assert!(
        refused.contains("may only write objects labelled"),
        "{refused}"
    );
    let kube_system = engine.call(
        None,
        &["delete", "namespace", "kube-system", "--dry-run=server"],
        None,
        Duration::from_secs(30),
    );
    assert!(
        !kube_system.ok(),
        "the engine account could delete kube-system"
    );

    // B9: the gateway and its writer sidecar; the first config is published through the writer.
    let dns = daemon_namespace_name(&engine.owner);
    let spec = GatewaySpec {
        namespace: dns.clone(),
        owner: engine.owner.clone(),
        image: crate::seat_gateway::GATEWAY_IMAGE.into(),
        writer_image: crate::seat_gateway::HELPER_IMAGE.into(),
        boot: "b1-live".into(),
    };
    cluster::apply_gateway(&engine, &spec, &[]).expect("gateway objects");
    let writer = cluster::writer_pod(&engine, &dns, Duration::from_secs(120)).expect("writer");
    let config = crate::seat_gateway::GatewayConfig::default();
    let mut touch = String::from("set -e; umask 077;");
    for r in &config.providers {
        if let Some(f) = r.credential_file.file_name().and_then(|f| f.to_str()) {
            touch.push_str(&format!(" [ -e /broker/{f} ] || : > /broker/{f};"));
        }
    }
    let body = serde_json::to_vec(&crate::seat_gateway::render(&config)).unwrap();
    let w = |script: &str, stdin: Option<&[u8]>| {
        let mut args = vec!["exec"];
        if stdin.is_some() {
            args.push("-i");
        }
        args.extend(["-c", "writer", writer.as_str(), "--", "sh", "-c", script]);
        engine.call(Some(&dns), &args, stdin, Duration::from_secs(60))
    };
    assert!(w(&touch, None).ok(), "broker files");
    let publish = w(
        "umask 077; cat > /cfg/.config.json.new && mv /cfg/.config.json.new /cfg/config.json",
        Some(&body),
    );
    assert!(publish.ok(), "{}", publish.err_text());
    assert!(w("printf b1-live > /cfg/boot", None).ok(), "the boot token");
    let gw =
        cluster::gateway_ready(&engine, &dns, Duration::from_secs(120)).expect("gateway serves");
    eprintln!(
        "[{:>5.1}s] gateway {} at {}",
        t0.elapsed().as_secs_f64(),
        gw.pod,
        gw.service_ip
    );
    // The readiness port answers inside the pod (the writer shares its network namespace), and
    // refuses a call with no unit key: the gateway serves its policy (review r1 P2).
    let ready = w(
        &format!(
            "wget -S -q -O /dev/null -T 3 http://127.0.0.1:{}/ 2>&1; true",
            crate::seat_gateway::READY_PORT
        ),
        None,
    );
    let ready = String::from_utf8_lossy(&ready.stdout).to_string();
    assert!(
        ready.contains(" 401 "),
        "the readiness listener did not refuse a keyless call: {ready}"
    );

    // B2: the pids limit, measured.
    let n = cluster::ensure_pids_limited(&engine, &floor).expect("pods are pids-limited");
    eprintln!(
        "[{:>5.1}s] pids probe: refused after {n}",
        t0.elapsed().as_secs_f64()
    );

    // A run namespace and one seat.
    let run = format!("live-{}", uuid::Uuid::new_v4().simple());
    let ns = cluster::ensure_run(&engine, &run, &RunBudget::default(), "1Gi").expect("run ns");
    let tree = PathBuf::from("/home/live/.wicked/repos/x/wt").join(&run);
    let sb = SeatPod {
        name: "seat-1".into(),
        namespace: ns.clone(),
        owner: engine.owner.clone(),
        run_label: label_value(&run),
        unit_label: "u1".into(),
        boot: boot_id().into(),
        role: "seat".into(),
        image: seat_image.clone(),
        floor_image: floor.clone(),
        workdir: tree.clone(),
        mounts: vec![ReplicaMount {
            path: tree.clone(),
            rw: true,
        }],
        memory_limit: "1Gi".into(),
        memory_request: "256Mi".into(),
        cpu_request: "100m".into(),
        gateway_ip: Some(gw.service_ip.clone()),
        shutdown_time: rfc3339_after(Duration::from_secs(15 * 60)),
        deadline_secs: 15 * 60,
        scope: None,
        view: None,
        deps: None,
    };
    let t_seat = std::time::Instant::now();
    engine
        .apply(&[render::seat_sandbox(&sb).unwrap()])
        .expect("seat sandbox");
    let mut ready = false;
    while t_seat.elapsed() < Duration::from_secs(120) {
        let pods = engine
            .list_owned(Some(&ns), "pods", &[(UNIT_LABEL, "u1")])
            .unwrap();
        if cluster::running_pod(&pods).is_some() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(ready, "the seat pod did not run");
    eprintln!(
        "[{:>5.1}s] seat pod running after {:.2}s",
        t0.elapsed().as_secs_f64(),
        t_seat.elapsed().as_secs_f64()
    );

    // B3: the engine's loop is PID 1, and survives the seat's kill -9 -1.
    let ps = exec(
        &engine,
        &ns,
        "seat-1",
        "seat",
        "cat /proc/1/cmdline | tr '\\0' ' '",
        None,
    );
    assert!(
        String::from_utf8_lossy(&ps.stdout).starts_with("/floor/busybox sh -c trap"),
        "{:?}",
        String::from_utf8_lossy(&ps.stdout)
    );
    let _ = exec(
        &engine,
        &ns,
        "seat-1",
        "seat",
        "/floor/busybox sleep 30 & kill -9 -1",
        None,
    );
    let alive = exec(&engine, &ns, "seat-1", "seat", "echo alive", None);
    assert!(alive.ok(), "the seat pod died on kill -9 -1");

    // B5 + F1: from the seat, only the gateway's seat ports answer. Every target below is a
    // LISTENING port (review r1 P2: a closed port proves nothing), and an exec that fails is an
    // error, never "closed".
    let probe = |host: &str, port: u16| -> bool {
        let r = exec(&engine, &ns, "seat-1", "seat",
            &format!("if /floor/busybox nc -w 3 {host} {port} </dev/null >/dev/null 2>&1; then echo open; else echo closed; fi"), None);
        let out = String::from_utf8_lossy(&r.stdout).trim().to_string();
        assert!(
            r.ok() && (out == "open" || out == "closed"),
            "probe {host}:{port} did not run: {}",
            r.err_text()
        );
        out == "open"
    };
    let env = |name: &str| {
        let r = exec(
            &engine,
            &ns,
            "seat-1",
            "seat",
            &format!("echo ${name}"),
            None,
        );
        String::from_utf8_lossy(&r.stdout).trim().to_string()
    };
    let api = env("KUBERNETES_SERVICE_HOST");
    assert!(!api.is_empty());
    let pod_json = engine
        .get_json(Some(&ns), "pod", "seat-1")
        .unwrap()
        .unwrap();
    let node_ip = pod_json["status"]["hostIP"].as_str().unwrap().to_string();
    let gw_pod = engine
        .get_json(Some(&dns), "pod", &gw.pod)
        .unwrap()
        .unwrap();
    let gw_ip = gw_pod["status"]["podIP"].as_str().unwrap().to_string();
    // The same targets answer from the gateway's own pod (so they listen) ...
    let listens = |host: &str, port: u16| {
        let r = w(&format!("if nc -w 3 {host} {port} </dev/null >/dev/null 2>&1; then echo open; else echo closed; fi"), None);
        String::from_utf8_lossy(&r.stdout).trim() == "open"
    };
    assert!(
        listens(&gw_ip, crate::seat_gateway::READY_PORT),
        "the readiness port does not listen"
    );
    eprintln!(
        "from the gateway pod: node {node_ip}:10250 open={}",
        listens(&node_ip, 10250)
    );
    // ... and are refused from the seat.
    assert!(!probe(&api, 443), "the API server is reachable from a seat");
    assert!(
        !probe(&node_ip, 10250),
        "the node's kubelet is reachable from a seat"
    );
    assert!(
        !probe(&node_ip, 6443),
        "the node's API port is reachable from a seat"
    );
    assert!(
        !probe("1.1.1.1", 443),
        "the internet is reachable from a seat"
    );
    assert!(
        !probe(&gw_ip, crate::seat_gateway::READY_PORT),
        "the gateway's readiness port is reachable from a seat"
    );
    assert!(
        probe("agw", 3000),
        "the gateway's CONNECT port is not reachable as agw"
    );

    // B4: the Landlock canary in the Sandbox pod, with the seat floor's own argv shape.
    let canary = exec(&engine, &ns, "seat-1", "seat",
        &format!("/floor/launch /wicked-canary/probe -- /floor/landrun --best-effort --ignore-missing \
                  --rox /usr --rox /bin --rox /lib --rox /etc --rox /floor --ro /proc --ro /dev \
                  --ro /wicked-canary --rw /dev/null --rwx /home/seat --rwx /tmp --rwx {t} \
                  --connect-tcp 3000 --env PATH --env HOME -- /floor/launch --check /wicked-canary/probe -- \
                  /floor/busybox sh -c 'touch {t}/ok && echo wrote-tree'", t = tree.display()), None);
    let err = String::from_utf8_lossy(&canary.stderr);
    assert!(err.contains("floor canary passed"), "{err}");
    assert!(String::from_utf8_lossy(&canary.stdout).contains("wrote-tree"));

    // C1: the sandbox carries its dead-man's switch; the run's deletion takes everything.
    let sbx: Value = engine
        .get_json(Some(&ns), "sandboxes.agents.x-k8s.io", "seat-1")
        .unwrap()
        .unwrap();
    assert_eq!(sbx["spec"]["shutdownPolicy"], "Delete");
    assert!(cluster::delete_run(&engine, &run, true).unwrap());
    assert!(engine.get_json(None, "namespace", &ns).unwrap().is_none());
    eprintln!(
        "[{:>5.1}s] run namespace deleted",
        t0.elapsed().as_secs_f64()
    );
}

/// Run a finalized seat command to its end, as `run_bounded` does (stdin closed).
fn run_seat(cmd: std::process::Command) -> (i32, String, String) {
    let mut cmd = cmd;
    cmd.stdin(std::process::Stdio::null());
    let out = cmd.output().expect("the kubectl client ran");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The seat argv a unit runs: the pod wrapper, the launcher under Landlock over `mounts`, then
/// `script` in the seat image's own shell.
fn seat_cmd(
    wrapper: &[String],
    cwd: &std::path::Path,
    script: &str,
    env: &[(&str, &str)],
) -> std::process::Command {
    let mut c = std::process::Command::new(&wrapper[0]);
    c.hardened()
        .args(&wrapper[1..])
        .args(["/bin/sh", "-c", script])
        .current_dir(cwd);
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

/// B-2 end to end on a real cluster (PARITY A2, A3, A12, B5, B8, B9, B11, B15, B17): the gateway on
/// its writer sidecar (publish, a unit key served then refused); a creator seat pod armed from a
/// real git worktree, its unit env over stdin (never on the argv), its change, deletion and
/// commit written back byte-identical with the host HEAD fast-forwarded; a planted `.git` hook
/// never coming back; a read-only unit's writes succeeding in the pod and vanishing; and a lost
/// exec stream read as no result, never an exit code.
#[test]
fn k8s_live_a_seat_pod_round_trip_writes_back_only_what_it_may() {
    let Some((admin_ctx, seat_image)) = live() else {
        return;
    };
    let t0 = std::time::Instant::now();
    let say = |m: &str| eprintln!("[{:>5.1}s] {m}", t0.elapsed().as_secs_f64());
    let home = std::env::temp_dir().join(format!(
        "wicked-k8s-live2-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let home = home.canonicalize().unwrap();
    let mut admin = resolve_settings(
        &home,
        None,
        None,
        Some(&admin_ctx),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        crate::validator::find_on_path,
    )
    .expect("kubectl and the admin context");
    admin.owner = owner_id(&home);
    let _cleanup = Uninstall(admin.clone());
    let rt = crate::container_floor::runtime().expect("the image builder answers");
    let floor = crate::container_floor::ensure_floor_image(&rt).expect("the floor image");
    let sync_image = crate::checks_container::image_tag();
    if !crate::container_floor::image_present(&rt, &sync_image) {
        crate::container_floor::build_image(
            &rt,
            crate::checks_container::DOCKERFILE,
            &sync_image,
            &[],
        )
        .expect("the sync image");
    }
    cluster::install(&admin, &home).expect("cluster install");
    let engine = resolve_settings(
        &home,
        None,
        None,
        None,
        None,
        crate::validator::find_on_path,
    )
    .expect("the engine's own kubeconfig");
    cluster::probe_cached(&engine).expect("probe");
    cluster::ensure_pids_limited(&engine, &floor).expect("pids");
    say("installed");

    // B8, B9: the gateway on its writer; a unit key is served before a seat starts, refused after.
    let gdir = home.join("gateway");
    std::fs::create_dir_all(&gdir).unwrap();
    let gw = super::gw::ensure(&engine, &gdir).expect("the gateway serves");
    say(&format!("gateway {} at {}", gw.pod, gw.service_ip));
    let key = crate::seat_gateway::UnitKey::new("live2", "live2#1", "codex");
    let t_key = std::time::Instant::now();
    super::gw::register_live(&gw, key.clone()).expect("the key is served");
    say(&format!(
        "key register -> served in {:.2}s",
        t_key.elapsed().as_secs_f64()
    ));
    super::gw::deregister_live(&gw, &key.key).expect("the key is refused");
    gw.write_file(super::gw::Volume::Broker, "live-probe", b"secret-bytes")
        .expect("a broker write");
    let back = gw
        .read_broker(&[("live-probe", false), ("missing-file", true)])
        .expect("a broker read");
    assert_eq!(
        back,
        vec![("live-probe".to_string(), b"secret-bytes".to_vec())]
    );

    // A real worktree: one commit, a file to change, one to delete, an ignored build dir.
    let tree = home.join("repo");
    std::fs::create_dir_all(&tree).unwrap();
    let git = |args: &[&str]| {
        let o = super::sync::git(&tree).args(args).output().unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(tree.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(tree.join("a.txt"), "before\n").unwrap();
    std::fs::write(tree.join("gone.txt"), "x\n").unwrap();
    std::fs::write(tree.join(".env"), "HOST_SECRET=1\n").unwrap();
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "base",
    ]);
    let base = git(&["rev-parse", "HEAD"]);
    let landrun = |rw: bool| -> Vec<String> {
        let mut v: Vec<String> = vec![
            "/floor/launch".into(),
            "/wicked-canary/probe".into(),
            "--".into(),
        ];
        v.extend(crate::container_floor::landrun_prefix(
            &[crate::container_floor::Mount::Bind {
                path: tree.clone(),
                rw,
            }],
            crate::container_floor::GATEWAY_PORTS,
            &[],
        ));
        v
    };
    let mounts = vec![
        super::seat::PlanMount::Bind {
            path: tree.clone(),
            rw: true,
        },
        super::seat::PlanMount::Mask {
            path: tree.join(".env"),
        },
    ];
    let spec = |view: bool, run: &'static str, ord: u32| super::seat::ArmSpec {
        run_id: run,
        generation: super::lifecycle(run),
        ord,
        cli: "codex",
        image: &seat_image,
        floor_image: &floor,
        sync_image: &sync_image,
        workdir: &tree,
        tree: &tree,
        view,
        mounts: &mounts,
        protected: &[],
        write_roots: if view {
            &[]
        } else {
            std::slice::from_ref(&tree)
        },
        seat_dirs: &[],
        scratch: None,
        seed_base: None,
        memory: "1Gi",
        gateway_ip: Some(&gw.service_ip),
        deadline: Duration::from_secs(600),
        role: "seat",
        deps: None,
    };

    // B11: a creator unit.
    let t_arm = std::time::Instant::now();
    let (seat, wrapper) = super::seat::arm(&engine, &spec(false, "live2-run", 1), landrun(true))
        .expect("the seat arms");
    say(&format!(
        "creator seat armed in {:.2}s (namespace, sync pod, copy-in, pod, deny check)",
        t_arm.elapsed().as_secs_f64()
    ));
    let script = "set -e; test ! -e .env; test \"$UNIT_SECRET\" = s3cr3t; \
                  echo after > a.txt; rm gone.txt; echo new > new.txt; mkdir -p target; echo o > target/out.o; \
                  printf '#!/bin/sh\\necho pwned\\n' > .git/hooks/post-commit; chmod +x .git/hooks/post-commit; \
                  git add -A; git commit -qm unit; echo > .env; echo done";
    // Like spawn hardening: an already resolved, seat-owned copy outside the mounted tree,
    // supplied explicitly on the carrier. The pod must receive its identity without rediscovering it.
    let global = home.join("gitconfig-seat-copy");
    let system = home.join("gitconfig-system");
    std::fs::write(
        &global,
        "[user]\n\tname = Op\n\temail = op@users.noreply.example\n",
    )
    .unwrap();
    std::fs::write(&system, "").unwrap();
    let mut cmd = seat_cmd(&wrapper, &tree, script, &[("UNIT_SECRET", "s3cr3t")]);
    cmd.env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_SYSTEM", &system)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            wicked_apps_core::spawn::NOPUSH_SSH_COMMAND,
        )
        .envs(wicked_apps_core::spawn::nopush_git_config_env());
    let (fin, seat2) = super::seat::finalize(&cmd)
        .expect("finalize")
        .expect("a pod seat");
    let argv: Vec<String> = fin
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        !argv.iter().any(|a| a.contains("s3cr3t")),
        "a secret rode the exec argv"
    );
    assert!(
        fin.get_envs().all(|(k, _)| k != "UNIT_SECRET"),
        "a secret is in kubectl's env"
    );
    assert_eq!(seat2.id, seat.id);
    let (code, out, err) = run_seat(fin);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(err.contains("floor canary passed"), "{err}");
    let code = super::seat::settle(&seat, super::seat::Ending::Exited(code)).expect("settled");
    assert_eq!(code, 0);
    say("creator unit settled");
    assert_eq!(
        std::fs::read_to_string(tree.join("a.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("new.txt")).unwrap(),
        "new\n"
    );
    assert!(!tree.join("gone.txt").exists(), "a deleted file came back");
    assert!(
        !tree.join("target").exists(),
        "ignored build output came back"
    );
    assert_eq!(
        std::fs::read_to_string(tree.join(".env")).unwrap(),
        "HOST_SECRET=1\n",
        "a mask was written"
    );
    assert!(
        !tree.join(".git/hooks/post-commit").exists(),
        "a planted hook came back"
    );
    let head = git(&["rev-parse", "HEAD"]);
    assert_ne!(head, base);
    assert_eq!(
        git(&["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
        "Op <op@users.noreply.example>|Op <op@users.noreply.example>",
        "the landed commit must carry the operator's identity for author and committer"
    );
    assert_eq!(
        git(&["rev-parse", "HEAD^"]),
        base,
        "not a fast-forward of the base"
    );
    assert_eq!(
        git(&["status", "--porcelain"]),
        "",
        "the host index does not match the commit"
    );
    let pods = engine
        .list_owned(Some(&seat.ns), "pods", &[(UNIT_LABEL, &seat.unit_label)])
        .unwrap();
    assert!(pods.is_empty(), "the seat pod outlived its settle");

    // B17: a read-only unit writes freely in its view; nothing comes back.
    let before = git(&["rev-parse", "HEAD^{tree}"]);
    let (ro, wrapper) = super::seat::arm(&engine, &spec(true, "live2-run", 2), landrun(true))
        .expect("the view arms");
    // Phase C: an evaluator's git works in its view (the tree root is the seat's own, never
    // "dubious ownership"), and sees the change it made there.
    let cmd = seat_cmd(&wrapper, &tree, "set -e; touch x; echo z > a.txt; python3 -c 'open(\"p\",\"w\").write(\"p\")' 2>/dev/null || true; cat a.txt; \
                  echo LOG=$(git log --format=%H -1); git diff --stat; git status --porcelain", &[]);
    let (fin, _) = super::seat::finalize(&cmd).unwrap().unwrap();
    let (code, out, err) = run_seat(fin);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains('z'), "the view was not writable: {out}");
    assert!(
        out.contains(&format!("LOG={head}")) && out.contains("a.txt |"),
        "git log/diff in a read-only unit: {out}\n{err}"
    );
    assert!(!err.contains("dubious ownership"), "{err}");
    super::seat::settle(&ro, super::seat::Ending::Exited(code)).expect("settled");
    assert_eq!(
        std::fs::read_to_string(tree.join("a.txt")).unwrap(),
        "after\n"
    );
    assert!(!tree.join("x").exists() && !tree.join("p").exists());
    assert_eq!(git(&["rev-parse", "HEAD^{tree}"]), before);
    assert_eq!(git(&["status", "--porcelain"]), "");
    say("read-only unit settled; host unchanged");

    // A3: a stream lost before the CLI ends (the client killed) is no result.
    let (lost, wrapper) =
        super::seat::arm(&engine, &spec(false, "live2-run", 3), landrun(true)).expect("arms");
    let cmd = seat_cmd(&wrapper, &tree, "sleep 30", &[]);
    let (mut fin, _) = super::seat::finalize(&cmd).unwrap().unwrap();
    fin.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = fin.spawn().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let _ = child.kill();
    let code = child.wait().unwrap().code().unwrap_or(-1);
    let r = super::seat::settle(&lost, super::seat::Ending::Exited(code));
    assert!(
        r.as_ref().is_err_and(|e| e.starts_with("exec_stream_lost")),
        "{r:?}"
    );
    say("a lost stream read as no result");
    assert!(cluster::delete_run(&engine, "live2-run", true).unwrap());
}

/// PR-B's deleted Docker-live proofs on real seat pods. These compile in ordinary CI; running
/// them needs the same opt-in and admin context as the round trips above.
#[cfg(unix)]
mod parity {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl EnvGuard {
        fn change(values: &[(&'static str, Option<&str>)]) -> Self {
            let guard = Self(
                values
                    .iter()
                    .map(|(k, _)| (*k, std::env::var_os(k)))
                    .collect(),
            );
            for (key, value) in values {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
            guard
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    struct Harness {
        kube: Kube,
        home: PathBuf,
        image: String,
        floor: String,
        _cleanup: Uninstall,
    }
    impl Harness {
        fn install(admin_ctx: &str) -> Self {
            let home = std::env::temp_dir().join(format!(
                "wicked-k8s-parity-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&home).unwrap();
            let home = home.canonicalize().unwrap();
            let mut admin = resolve_settings(
                &home,
                None,
                None,
                Some(admin_ctx),
                std::env::var_os("HOME").map(PathBuf::from).as_deref(),
                crate::validator::find_on_path,
            )
            .expect("kubectl and the admin context");
            admin.owner = owner_id(&home);
            let cleanup = Uninstall(admin.clone());
            let rt = crate::container_floor::runtime().expect("the image builder answers");
            let floor = crate::container_floor::ensure_floor_image(&rt).expect("floor image");
            // The pinned CLI, independent of the general round-trip's optional codex image.
            let image = {
                let _image = EnvGuard::change(&[("WICKED_SEAT_IMAGE_CLAUDE", None)]);
                crate::container_floor::ensure_seat_image(&rt, "claude")
                    .expect("the pinned Claude seat image")
            };
            cluster::install(&admin, &home).expect("cluster install");
            let kube = resolve_settings(
                &home,
                None,
                None,
                None,
                None,
                crate::validator::find_on_path,
            )
            .expect("the engine's scoped account");
            cluster::probe(&kube).expect("the cluster enforces the pod floor");
            Self {
                kube,
                home,
                image,
                floor,
                _cleanup: cleanup,
            }
        }

        fn seat(&self, gateway: Option<&str>) -> String {
            let run = format!("parity-{}", uuid::Uuid::new_v4().simple());
            let ns = cluster::ensure_run(&self.kube, &run, &RunBudget::default(), "1Gi")
                .expect("throwaway run namespace");
            let spec = SeatPod {
                name: "seat-1".into(),
                namespace: ns.clone(),
                owner: self.kube.owner.clone(),
                run_label: label_value(&run),
                unit_label: "parity".into(),
                boot: boot_id().into(),
                role: "seat".into(),
                image: self.image.clone(),
                floor_image: self.floor.clone(),
                workdir: PathBuf::from("/home/seat"),
                mounts: vec![],
                memory_limit: "2Gi".into(),
                memory_request: "256Mi".into(),
                cpu_request: "100m".into(),
                gateway_ip: gateway.map(str::to_string),
                shutdown_time: rfc3339_after(Duration::from_secs(20 * 60)),
                deadline_secs: 20 * 60,
                scope: None,
                view: None,
                deps: None,
            };
            let mut sandbox = render::seat_sandbox(&spec).unwrap();
            // Synthetic fenced files live only in this pod's scratch, never on the host.
            let pod = &mut sandbox["spec"]["podTemplate"]["spec"];
            pod["volumes"].as_array_mut().unwrap().push(json!({
                "name": "fence", "emptyDir": {"medium": "Memory", "sizeLimit": "256Mi"}
            }));
            pod["containers"][0]["volumeMounts"]
                .as_array_mut()
                .unwrap()
                .push(json!({
                    "name": "fence", "mountPath": "/fence"
                }));
            self.kube.apply(&[sandbox]).expect("seat Sandbox");
            let start = std::time::Instant::now();
            loop {
                let pods = self
                    .kube
                    .list_owned(Some(&ns), "pods", &[(UNIT_LABEL, "parity")])
                    .unwrap();
                if cluster::running_pod(&pods).is_some() {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(120),
                    "seat pod did not run"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            self.run(&ns, "mkdir -p /home/seat/work", None);
            ns
        }

        fn run(&self, ns: &str, script: &str, stdin: Option<&[u8]>) -> String {
            let mut args = vec!["exec"];
            if stdin.is_some() {
                args.push("-i");
            }
            args.extend([
                "-c",
                "seat",
                "seat-1",
                "--",
                "/floor/busybox",
                "sh",
                "-c",
                script,
            ]);
            let out = self
                .kube
                .call(Some(ns), &args, stdin, Duration::from_secs(300));
            assert!(out.ok(), "pod exec failed: {}", out.err_text());
            String::from_utf8(out.stdout).unwrap()
        }

        fn put(&self, ns: &str, name: &str, body: &[u8]) {
            assert!(name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)));
            self.run(
                ns,
                &format!("umask 077; cat > /home/seat/work/{name}"),
                Some(body),
            );
        }
    }

    /// The old stub.py: one requested tool call per turn and its result, including streaming.
    /// CALLS is a separate JSON file so fence and hook scenarios use the identical upstream.
    const ANTHROPIC_STUB: &str = r#"import json
from http.server import BaseHTTPRequestHandler, HTTPServer
with open('/home/seat/work/calls.json') as f: CALLS = json.load(f)
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self): self.reply({})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get('content-length', 0))) or b'{}')
        if 'count_tokens' in self.path: return self.reply({'input_tokens': 1})
        res = [c for m in body.get('messages', []) if m.get('role') == 'user' and isinstance(m.get('content'), list) for c in m['content'] if c.get('type') == 'tool_result']
        main = any(t.get('name') in ('Read', 'Bash') for t in body.get('tools') or [])
        if res:
            with open('/home/seat/work/stub.log', 'a') as f: f.write('RESULT %d %s\n' % (len(res) - 1, json.dumps(res[-1])))
        step = len(res)
        if main and step < len(CALLS):
            blocks, stop = [{'type':'tool_use','id':'toolu_%d'%step,'name':CALLS[step][0],'input':CALLS[step][1]}], 'tool_use'
        else:
            blocks, stop = [{'type':'text','text':'STUB-DONE'}], 'end_turn'
        msg = {'id':'msg_%d'%step,'type':'message','role':'assistant','model':body.get('model','m'),'content':blocks,'stop_reason':stop,'stop_sequence':None,'usage':{'input_tokens':1,'output_tokens':1}}
        if not body.get('stream'): return self.reply(msg)
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.end_headers()
        def ev(t,d): self.wfile.write(('event: %s\ndata: %s\n\n'%(t,json.dumps(dict(type=t,**d)))).encode())
        ev('message_start', {'message':dict(msg,content=[],stop_reason=None)})
        for i,b in enumerate(blocks):
            if b['type'] == 'text':
                ev('content_block_start',{'index':i,'content_block':{'type':'text','text':''}})
                ev('content_block_delta',{'index':i,'delta':{'type':'text_delta','text':b['text']}})
            else:
                ev('content_block_start',{'index':i,'content_block':dict(b,input={})})
                ev('content_block_delta',{'index':i,'delta':{'type':'input_json_delta','partial_json':json.dumps(b['input'])}})
            ev('content_block_stop',{'index':i})
        ev('message_delta',{'delta':{'stop_reason':stop,'stop_sequence':None},'usage':{'output_tokens':1}})
        ev('message_stop',{})
    def reply(self,d):
        b=json.dumps(d).encode(); self.send_response(200); self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b)
HTTPServer(('127.0.0.1',8999),H).serve_forever()
"#;

    fn claude_argv(tree: &str, narrow: bool) -> Vec<String> {
        let _vars = EnvGuard::change(&[
            ("HOME", Some("/fence")),
            ("CLAUDE_CONFIG_DIR", None),
            (wicked_apps_core::spawn::INHERIT_OPERATOR_CONFIG_ENV, None),
        ]);
        let invocation =
            "claude -p --settings /home/seat/work/settings.json --model claude-sonnet-4-5 {PROMPT}";
        let mut argv = crate::execute_wrapped::build_argv(invocation, "do the steps", &[]);
        crate::execute_wrapped::inject_isolation_flags(
            &mut argv,
            invocation,
            None,
            Some("do the steps"),
            None,
            Some(Path::new(tree)),
        )
        .expect("the engine's isolation argv");
        let rules = argv
            .windows(2)
            .find(|w| w[0] == "--disallowedTools")
            .unwrap();
        assert!(
            rules[1].contains("Read(//fence/.ssh/**)")
                && rules[1].contains("Edit(//fence/.ssh/**)")
        );
        if narrow {
            let bind = |path: &str, rw| crate::container_floor::Mount::Bind {
                path: path.into(),
                rw,
            };
            let view = crate::container_floor::mount_view(
                &[
                    bind("/fence/.wicked/repos/x", false),
                    bind(tree, true),
                    bind("/fence/.wicked/walkthroughs", false),
                ],
                &[tree.into(), "/fence/.wicked/repos/x".into()],
            );
            crate::execute_wrapped::narrow_container_fence(
                &mut argv,
                Some("do the steps"),
                None,
                None,
                None,
                &[],
                &view,
            )
            .expect("the seat's working tree is opened in the fence");
        }
        argv
    }

    fn run_claude(h: &Harness, ns: &str, tree: &str, argv: &[String], calls: Value) -> String {
        h.put(ns, "stub.py", ANTHROPIC_STUB.as_bytes());
        h.put(ns, "calls.json", &serde_json::to_vec(&calls).unwrap());
        // Args are shell-quoted (empty setting scopes are significant); no credential is on
        // the API server's exec argv. The synthetic token exists only in the pod's script.
        let command = argv
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        h.run(
            ns,
            &format!(
                r#"set -e
cd {tree}
rm -f /home/seat/work/stub.log /home/seat/work/hook.log
python3 /home/seat/work/stub.py >/home/seat/work/server.log 2>&1 & server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
n=0; until /floor/busybox wget -q -O /dev/null http://127.0.0.1:8999/; do
  n=$((n+1)); [ "$n" -lt 100 ] || exit 1; sleep 0.1
done
set +e
ANTHROPIC_BASE_URL=http://127.0.0.1:8999 ANTHROPIC_API_KEY=sk-ant-dummy-not-a-token \
 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_AUTOUPDATER=1 HOME=/home/seat \
 /floor/busybox timeout 180 {command} >/home/seat/work/claude.out 2>&1
rc=$?
echo "CLAUDE_EXIT $rc"
echo "HOOK_FIRED $(wc -l </home/seat/work/hook.log 2>/dev/null || echo 0)"
cat /home/seat/work/stub.log
cat /home/seat/work/claude.out
exit 0
"#
            ),
            None,
        )
    }

    fn result(log: &str, step: usize) -> &str {
        log.lines()
            .find_map(|l| l.strip_prefix(&format!("RESULT {step} ")))
            .unwrap_or_else(|| panic!("missing tool result {step}: {log}"))
    }

    #[test]
    fn k8s_live_claudes_absolute_fence_holds_behind_hook_allow_and_opens_its_tree() {
        let Some((admin_ctx, _)) = live() else {
            return;
        };
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let h = Harness::install(&admin_ctx);
        let ns = h.seat(None);
        h.run(&ns, "mkdir -p /fence/.ssh /fence/open /fence/.wicked/walkthroughs /fence/.wicked/repos/x/wicked-worktrees/r; \
            echo SECRET-1029 > /fence/.ssh/id_rsa; echo SECRET-1029 > /fence/.git-credentials; \
            echo SECRET-1029 > /fence/.wicked/walkthroughs/w.md; echo OPEN-1029 > /fence/open/ok.md", None);
        h.put(&ns, "hook.sh", br#"#!/bin/sh
cat >/dev/null
echo fired >>/home/seat/work/hook.log
printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"stub gate allows"}}'
"#);
        h.run(&ns, "chmod +x /home/seat/work/hook.sh", None);
        h.put(
            &ns,
            "settings.json",
            &serde_json::to_vec(&json!({"hooks":{"PreToolUse":[{
                "matcher":"*", "hooks":[{"type":"command", "command":"/home/seat/work/hook.sh"}]
            }]}}))
            .unwrap(),
        );
        let log = run_claude(
            &h,
            &ns,
            "/fence/open",
            &claude_argv("/fence/open", false),
            json!([
                ["Read", {"file_path":"/fence/.ssh/id_rsa"}],
                ["Read", {"file_path":"/fence/.git-credentials"}],
                ["Read", {"file_path":"/fence/open/ok.md"}],
                ["Write", {"file_path":"/fence/.ssh/planted", "content":"W"}],
                ["Write", {"file_path":"/fence/open/new.md", "content":"W"}]
            ]),
        );
        for step in [0, 1, 3] {
            assert!(
                result(&log, step).contains("denied by your permission settings"),
                "{log}"
            );
            assert!(!result(&log, step).contains("SECRET-1029"), "{log}");
        }
        assert!(result(&log, 2).contains("OPEN-1029"), "{log}");
        assert!(result(&log, 4).contains("File created"), "{log}");
        h.run(
            &ns,
            "test ! -e /fence/.ssh/planted && test -f /fence/open/new.md",
            None,
        );
        assert!(
            log.contains("HOOK_FIRED 2"),
            "only allowed calls reached the hook: {log}"
        );
        assert!(
            log.contains("CLAUDE_EXIT 0") && log.contains("STUB-DONE"),
            "{log}"
        );

        const TREE: &str = "/fence/.wicked/repos/x/wicked-worktrees/r";
        h.run(&ns, &format!("echo OPEN-SF1019 > {TREE}/ok.md"), None);
        let log = run_claude(
            &h,
            &ns,
            TREE,
            &claude_argv(TREE, true),
            json!([
                ["Read", {"file_path":format!("{TREE}/ok.md")}],
                ["Write", {"file_path":format!("{TREE}/new.md"), "content":"W"}],
                ["Read", {"file_path":"/fence/.wicked/walkthroughs/w.md"}],
                ["Read", {"file_path":"/fence/.ssh/id_rsa"}],
                ["Bash", {"command":format!("cat {TREE}/ok.md"), "description":"read the tree"}]
            ]),
        );
        for step in [0, 4] {
            assert!(result(&log, step).contains("OPEN-SF1019"), "{log}");
            assert!(
                !result(&log, step).contains("denied by your permission settings"),
                "{log}"
            );
        }
        assert!(result(&log, 1).contains("File created"), "{log}");
        h.run(&ns, &format!("test -f {TREE}/new.md"), None);
        for step in [2, 3] {
            assert!(
                result(&log, step).contains("denied by your permission settings"),
                "{log}"
            );
            assert!(!result(&log, step).contains("SECRET-1029"), "{log}");
        }
        assert!(log.contains("HOOK_FIRED 3"), "{log}");
        assert!(
            log.contains("CLAUDE_EXIT 0") && log.contains("STUB-DONE"),
            "{log}"
        );
    }

    #[test]
    fn k8s_live_claudes_command_hook_is_judged_retried_and_revoked_through_the_gateway() {
        let Some((admin_ctx, _)) = live() else {
            return;
        };
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let h = Harness::install(&admin_ctx);
        let calls = h.home.join("hook-calls.ndjson");
        let killed = h.home.join("killed");
        let gate = h.home.join("gate-hook.sh");
        let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        crate::test_env::install_stub(&gate, &format!(
            "#!/bin/sh\n[ \"$1\" = gate-hook ] || exit 3\np=$(cat)\nprintf '%s\\n' \"$p\" >> {calls}\n\
             case \"$p\" in\n\
             *DENIED.txt*) echo 'DENIED-BY-THE-RELAYED-GATE' >&2; exit 2;;\n\
             *AFTER-KILL.txt*) [ -e {killed} ] || {{ : > {killed}; kill -9 $$; }};;\n\
             *FORCED.txt*) kill -9 $$;;\nesac\nexit 0\n",
            calls = quote(&calls), killed = quote(&killed),
        ));
        let _vars = EnvGuard::change(&[("WICKED_CORE_EXE", Some(gate.to_str().unwrap()))]);
        let dir = h.home.join("gateway");
        let gw = super::super::gw::ensure(&h.kube, &dir).expect("gateway and host relay route");
        assert!(
            crate::seat_gateway::hook_route_ready(),
            "the gateway must reach the host relay"
        );
        let ns = h.seat(Some(&gw.service_ip));
        let token = crate::hook_relay::mint_token();
        let decisions = h.home.join("decisions.ndjson");
        let registration = crate::hook_relay::register(
            &token,
            crate::hook_relay::HookContext {
                cwd: Some(h.home.clone()),
                env: vec![
                    (
                        crate::gate_hook::DECISIONS_PATH_ENV.into(),
                        Some(decisions.clone().into_os_string()),
                    ),
                    (
                        crate::gate_hook::GATE_SCOPE_ENV.into(),
                        Some("wicked-agent/pod-parity/shared".into()),
                    ),
                    (
                        crate::gate_hook::GATE_PHASE_ENV.into(),
                        Some("unit-1".into()),
                    ),
                ],
            },
        )
        .expect("the relay registers the unit");
        h.put(
            &ns,
            "settings.json",
            &serde_json::to_vec(&json!({
                "disableAllHooks":false, "hooks":{"PreToolUse":[{"matcher":"*", "hooks":[
                    crate::execute_wrapped::container_gate_hook(&token).unwrap()
                ]}]}
            }))
            .unwrap(),
        );
        let argv: Vec<String> = [
            "claude",
            "-p",
            "run the commands",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "claude-sonnet-4-5",
            "--setting-sources",
            "",
            "--permission-mode",
            "acceptEdits",
            "--settings",
            "/home/seat/work/settings.json",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let commands = json!([
            ["Bash", {"command":"echo allowed > ALLOWED.txt", "description":"allowed"}],
            ["Bash", {"command":"echo denied > DENIED.txt", "description":"denied"}],
            ["Bash", {"command":"for m in a b c; do sh -c 'i=0; while [ $i -lt 4000 ]; do echo test-$i; i=$((i+1)); done; sleep 15; echo complete' | tail -1; done; echo done > LONG.txt", "description":"long loop", "timeout":300000}],
            ["Bash", {"command":"echo retried > AFTER-KILL.txt", "description":"retry"}],
            ["Bash", {"command":"echo forced > FORCED.txt", "description":"fail closed"}]
        ]);
        let log = run_claude(&h, &ns, "/home/seat", &argv, commands);
        h.run(&ns, "test -f /home/seat/ALLOWED.txt && test ! -e /home/seat/DENIED.txt && \
            test -f /home/seat/LONG.txt && test -f /home/seat/AFTER-KILL.txt && test ! -e /home/seat/FORCED.txt", None);
        let judged = std::fs::read_to_string(&calls).unwrap();
        assert!(
            judged.contains("ALLOWED.txt") && judged.contains("DENIED.txt"),
            "{judged}"
        );
        assert_eq!(
            judged.matches("AFTER-KILL.txt").count(),
            2,
            "killed once, judged twice"
        );
        assert_eq!(
            judged.matches("FORCED.txt").count(),
            2,
            "a second failure is final"
        );
        assert!(
            log.contains("DENIED-BY-THE-RELAYED-GATE") && log.contains("SIGKILL"),
            "{log}"
        );
        assert!(!log.contains("HTTP hook blocked"), "{log}");
        assert!(
            log.contains("CLAUDE_EXIT 0") && log.contains("STUB-DONE"),
            "{log}"
        );
        assert!(
            !registration.finish(),
            "every infrastructure refusal was recorded"
        );
        let records = crate::gate_hook::collect_hook_decisions_in(&decisions, "unit-1");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].decision, "deny");
        assert_eq!(records[0].tool_name, "Bash");
        assert_eq!(
            records[0].denying_policy.as_deref(),
            Some("engine:governance-infra")
        );
        assert!(records[0]
            .obligations
            .first()
            .is_some_and(|r| r.contains("SIGKILL")));

        // Same command hook and token after the registration ends: no host hook or tool runs.
        h.run(&ns, "rm /home/seat/ALLOWED.txt", None);
        let before = std::fs::read(&calls).unwrap();
        let revoked = run_claude(
            &h,
            &ns,
            "/home/seat",
            &argv,
            json!([
                ["Bash", {"command":"echo allowed > ALLOWED.txt", "description":"revoked"}]
            ]),
        );
        h.run(&ns, "test ! -e /home/seat/ALLOWED.txt", None);
        assert_eq!(
            std::fs::read(&calls).unwrap(),
            before,
            "a revoked request never reaches the gate"
        );
        assert!(
            revoked.contains("unknown or revoked hook token"),
            "{revoked}"
        );
    }

    /// A synthetic provider in the gateway's own network namespace. Only the gateway can reach
    /// loopback; neither seat gains an egress exception for the upstream.
    const ECHO_UPSTREAM: &str = r#"import json
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        self.rfile.read(int(self.headers.get('content-length', 0)))
        body=json.dumps({'auth':self.headers.get('authorization',''),
            'key':self.headers.get('x-api-key',''), 'unit':self.headers.get('x-wicked-unit',''),
            'gemini':self.headers.get('x-goog-api-key',''), 'acct':self.headers.get('chatgpt-account-id','')}).encode()
        self.send_response(200); self.send_header('content-type','application/json')
        self.send_header('content-length',str(len(body))); self.end_headers(); self.wfile.write(body)
HTTPServer(('127.0.0.1',8998),H).serve_forever()
"#;

    fn add_echo_upstream(h: &Harness, gw: &super::super::gw::Gw) -> super::super::gw::Gw {
        let deployment = h
            .kube
            .get_json(Some(&gw.ns), "deployment", render::GATEWAY_NAME)
            .unwrap()
            .unwrap();
        let mut spec = deployment["spec"].clone();
        let security = spec["template"]["spec"]["containers"][0]["securityContext"].clone();
        spec["template"]["spec"]["containers"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "name":"upstream", "image":h.image, "imagePullPolicy":"Never",
                "command":["python3", "-u", "-c", ECHO_UPSTREAM], "securityContext":security,
                "resources":{"requests":{"memory":"32Mi","cpu":"10m"},"limits":{"memory":"128Mi"}}
            }));
        h.kube
            .apply(&[json!({
                "apiVersion":"apps/v1", "kind":"Deployment", "spec":spec,
                "metadata":{"name":render::GATEWAY_NAME,"namespace":gw.ns,
                    "labels":deployment["metadata"]["labels"]}
            })])
            .expect("test upstream sidecar");
        let start = std::time::Instant::now();
        loop {
            let pods = h
                .kube
                .list_owned(Some(&gw.ns), "pods", &[(ROLE_LABEL, "gateway")])
                .unwrap();
            if let Some(pod) = pods.iter().find(|p| {
                p["metadata"]["deletionTimestamp"].is_null()
                    && p["status"]["containerStatuses"]
                        .as_array()
                        .is_some_and(|containers| {
                            containers.iter().any(|c| c["name"] == "upstream")
                                && containers.iter().all(|c| c["ready"] == true)
                        })
            }) {
                let mut current = gw.clone();
                current.pod = pod["metadata"]["name"].as_str().unwrap().into();
                return current;
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "gateway sidecar did not run"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn ask_route(
        h: &Harness,
        ns: &str,
        route: &crate::seat_gateway::ModelRoute,
        unit: &str,
    ) -> (u16, String) {
        use crate::seat_credentials::CredentialKind;
        let claude_oauth =
            route.owner() == Some("claude") && route.kind == Some(CredentialKind::ClaudeOauth);
        let header = route
            .unit_header
            .as_deref()
            .or(claude_oauth.then_some(crate::seat_gateway::UNIT_HEADER))
            .unwrap_or(match route.kind {
                Some(CredentialKind::ClaudeApiKey) => "x-api-key",
                Some(CredentialKind::GeminiApiKey) => crate::seat_gateway::GEMINI_KEY_HEADER,
                _ => "authorization",
            });
        let prefix = if header == "authorization" {
            "Bearer "
        } else {
            ""
        };
        let extra = if claude_oauth {
            // The seat's placeholder record's bearer, and a stray API key (wicked-garden#1365).
            "-H 'authorization: Bearer PLACEHOLDER-not-a-token' -H 'x-api-key: FAKE-stray-key'"
        } else if header == crate::seat_gateway::UNIT_HEADER
            && route.kind != Some(CredentialKind::ClaudeApiKey)
        {
            "-H 'authorization: Bearer PLACEHOLDER-not-a-token'"
        } else {
            ""
        };
        let account = if route.provider == crate::seat_gateway::Provider::ChatGpt {
            "-H 'chatgpt-account-id: acct-PLACEHOLDER'"
        } else {
            ""
        };
        let script = format!(
            "read K; curl -sS --max-time 6 -w '\\n%{{http_code}}' \
             -H \"{header}: {prefix}$K\" {extra} {account} \
             -H 'content-type: application/json' -d '{{}}' http://agw:{}/probe-{}",
            route.port, route.port,
        );
        let reply = h.run(ns, &script, Some(format!("{unit}\n").as_bytes()));
        let (body, status) = reply.rsplit_once('\n').expect("curl status");
        (status.trim().parse().unwrap(), body.into())
    }

    fn route_eventually(
        h: &Harness,
        ns: &str,
        route: &crate::seat_gateway::ModelRoute,
        unit: &str,
        want: impl Fn(u16, &str) -> bool,
    ) -> (u16, String) {
        let start = std::time::Instant::now();
        loop {
            let (status, body) = ask_route(h, ns, route, unit);
            if want(status, &body) {
                return (status, body);
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "route {}: HTTP {status}: {body}",
                route.port
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    struct StubCliGuard(Option<PathBuf>);
    impl Drop for StubCliGuard {
        fn drop(&mut self) {
            *crate::seat_refresh::TEST_CLI_DIR.lock().unwrap() = self.0.take();
        }
    }

    #[test]
    fn k8s_live_credentials_are_owner_scoped_refreshed_signed_out_and_absent_from_logs() {
        use super::super::gw::{self, Volume};
        use crate::seat_credentials::CredentialKind;
        use crate::seat_gateway::{GatewayConfig, Provider, UnitKey};
        use crate::seat_login::{BrokerIo, BrokerStatus};
        let Some((admin_ctx, _)) = live() else {
            return;
        };
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let h = Harness::install(&admin_ctx);
        let dir = h.home.join("gateway");
        let mut config = GatewayConfig::default();
        for route in &mut config.providers {
            route.upstream_host = "127.0.0.1:8998".into();
            route.tls = false;
            if route.provider == Provider::ChatGpt {
                config.chatgpt_accounts.insert(
                    route
                        .credential_file
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .into(),
                    format!("acct-FAKE-{}", route.port),
                );
            }
        }
        crate::seat_gateway::configure_gateway(&dir, &config).unwrap();
        let gateway = gw::ensure(&h.kube, &dir).expect("gateway");
        let gateway = add_echo_upstream(&h, &gateway);
        let io = super::super::login::K8sIo::of(&gateway);
        let seat_a = h.seat(Some(&gateway.service_ip));
        let seat_b = h.seat(Some(&gateway.service_ip));
        let mut units = std::collections::BTreeMap::new();
        for cli in ["claude", "codex", "pi", "opencode", "copilot", "agy"] {
            let key = UnitKey::new("credential-parity", &format!("unit-{cli}"), cli);
            gw::register_live(&gateway, key.clone()).expect("owner key served");
            units.insert(cli, key);
        }
        let mut secrets = vec![
            "wk-FAKE-unknown".to_string(),
            "FAKE-stray-key".into(),
            "PLACEHOLDER-not-a-token".into(),
        ];
        for route in &config.providers {
            let file = route.credential_file.file_name().unwrap().to_str().unwrap();
            let secret = format!("FAKE-broker-{}", route.port);
            gateway
                .write_file(Volume::Broker, file, secret.as_bytes())
                .unwrap();
            secrets.push(secret);
            if route.provider == Provider::ChatGpt {
                let account = route.credential_file.with_extension("account");
                gateway
                    .write_file(
                        Volume::Broker,
                        account.file_name().unwrap().to_str().unwrap(),
                        format!("acct-FAKE-{}", route.port).as_bytes(),
                    )
                    .unwrap();
            }
        }
        // An actual retained codex record lets the real state machine run its host-CLI renewal.
        let record = json!({"auth_mode":"chatgpt", "OPENAI_API_KEY":null, "tokens":{
            "access_token":"FAKE-broker-4002", "refresh_token":"FAKE-codex-refresh",
            "id_token":"FAKE-codex-id", "account_id":"acct-FAKE-4002"
        }, "last_refresh":"2026-01-01T00:00:00Z"});
        gateway
            .write_file(
                Volume::Broker,
                "codex.auth.json",
                &serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
        secrets.extend(["FAKE-codex-refresh".into(), "FAKE-codex-id".into()]);

        for route in &config.providers {
            let owner = route.owner().unwrap();
            let unit = &units[owner];
            let ns = if owner == "opencode" {
                &seat_b
            } else {
                &seat_a
            };
            let expected = format!("FAKE-broker-{}", route.port);
            let claude_oauth = owner == "claude" && route.kind == Some(CredentialKind::ClaudeOauth);
            let expected_auth = &expected;
            let (_, body) = route_eventually(&h, ns, route, &unit.key, |status, body| {
                status == 200 && body.contains(expected_auth)
            });
            let upstream: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(upstream["unit"], "", "the unit header is stripped");
            assert!(
                !body.contains(&unit.key) && !body.contains("PLACEHOLDER"),
                "{body}"
            );
            match route.kind.unwrap() {
                CredentialKind::ClaudeApiKey => assert_eq!(upstream["key"], expected),
                CredentialKind::GeminiApiKey => assert_eq!(upstream["gemini"], expected),
                _ => assert_eq!(upstream["auth"], format!("Bearer {expected_auth}")),
            }
            if claude_oauth {
                // wicked-garden#1365: the broker's setup-token replaces the seat's placeholder.
                assert_eq!(upstream["key"], "", "a stray API key is stripped");
            }
            if route.provider == Provider::ChatGpt {
                assert_eq!(upstream["acct"], format!("acct-FAKE-{}", route.port));
            }
            // Both are gateway refusals: the echo upstream answers every request with 200.
            let foreign = if owner == "pi" { "opencode" } else { "pi" };
            let foreign_seat = if foreign == "opencode" {
                &seat_b
            } else {
                &seat_a
            };
            assert_eq!(
                ask_route(&h, foreign_seat, route, &units[foreign].key).0,
                401,
                "{foreign}'s seat must not use {owner}'s route {}",
                route.port
            );
            assert_eq!(ask_route(&h, ns, route, "wk-FAKE-unknown").0, 401);
        }

        let bin = dir.join("bin");
        crate::seat_refresh::install_stub_clis(
            &bin,
            &[("FAKE-broker-4002", "FAKE-refreshed-4002")],
        );
        let _stub_cli = StubCliGuard(
            crate::seat_refresh::TEST_CLI_DIR
                .lock()
                .unwrap()
                .replace(bin),
        );
        assert!(matches!(
            crate::seat_refresh::refresh(&io, &dir, "codex", true).unwrap(),
            crate::seat_refresh::Outcome::Refreshed { .. }
        ));
        let codex = config.providers.iter().find(|r| r.port == 4002).unwrap();
        route_eventually(&h, &seat_a, codex, &units["codex"].key, |status, body| {
            status == 200 && body.contains("Bearer FAKE-refreshed-4002")
        });
        assert_eq!(
            crate::seat_login::broker_status(&io, "codex"),
            BrokerStatus::SignedIn { expires_at: None }
        );
        secrets.push("FAKE-refreshed-4002".into());

        for (owner, unit) in &units {
            crate::seat_login::sign_out_and_route(&io, &dir, owner).expect("sign-out confirmed");
            assert_eq!(
                crate::seat_login::broker_status(&io, owner),
                BrokerStatus::SignedOut
            );
            let files: Vec<_> = config
                .providers
                .iter()
                .filter(|r| r.owner() == Some(*owner))
                .map(|r| r.credential_file.file_name().unwrap().to_str().unwrap())
                .collect();
            let emptied = io.read_broker(&files, false).unwrap();
            assert_eq!(emptied.len(), files.len());
            assert!(
                emptied.iter().all(|(_, value)| value.is_empty()),
                "{owner}'s broker files are empty"
            );
            for route in config
                .providers
                .iter()
                .filter(|r| r.owner() == Some(*owner))
            {
                if *owner == "claude" && route.kind == Some(CredentialKind::ClaudeOauth) {
                    continue;
                }
                // The unit is still live: sign-out itself must stop broker credentials and
                // account literals reaching the upstream, without relying on key revocation.
                route_eventually(&h, &seat_a, route, &unit.key, |status, body| {
                    status != 200 || !body.contains("FAKE-")
                });
            }
            // End the unit as well: its key stops admitting any route.
            gw::deregister_live(&gateway, &unit.key).unwrap();
            for route in config
                .providers
                .iter()
                .filter(|r| r.owner() == Some(*owner))
            {
                assert_eq!(ask_route(&h, &seat_a, route, &unit.key).0, 401);
            }
        }
        let logs = h.kube.call(
            Some(&gateway.ns),
            &["logs", &gateway.pod, "-c", "gateway"],
            None,
            Duration::from_secs(30),
        );
        assert!(logs.ok(), "gateway logs: {}", logs.err_text());
        let logs = format!(
            "{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
        for secret in secrets.iter().chain(units.values().map(|u| &u.key)) {
            assert!(
                !logs.contains(secret),
                "a credential reached the gateway log"
            );
        }
    }
}
