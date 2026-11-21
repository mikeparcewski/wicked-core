//! Engine-built images enter the node's store at use time. Never pull from a registry.

use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use wicked_apps_core::HardenedCommand;

use super::{cluster::NodeInfo, Kube};
use crate::container_floor::{image_id, RemoveOnDrop, Runtime};
use crate::worker_sandbox::FloorUnarmed;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageStore {
    Shared,
    Kind { cluster: String },
    K3s,
}

/// A save or a load of a multi-GiB seat image on a loaded host (once per image id per node).
pub(crate) const IMAGE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

const UNKNOWN_FIX: &str = "select nerdctl on the node's containerd (in namespace k8s.io); \
    the builder's image store is not known to be shared with this node";

impl ImageStore {
    /// Only node facts and the selected builder determine the store. Missing facts refuse.
    pub(crate) fn classify(node: &Value, nerdctl: bool) -> Result<Self, (FloorUnarmed, String)> {
        let provider = node
            .pointer("/spec/providerID")
            .and_then(Value::as_str)
            .unwrap_or("");
        let info = &node["status"]["nodeInfo"];
        let runtime = info["containerRuntimeVersion"].as_str().unwrap_or("");
        if let Some(kind) = provider.strip_prefix("kind://") {
            let parts: Vec<_> = kind.split('/').collect();
            return match parts.as_slice() {
                [provider, cluster, node]
                    if [provider, cluster, node].iter().all(|s| !s.is_empty()) =>
                {
                    Ok(Self::Kind {
                        cluster: cluster.to_string(),
                    })
                }
                _ => Err((
                    FloorUnarmed::ImageUnavailable,
                    format!("invalid kind providerID {provider:?}; {UNKNOWN_FIX}"),
                )),
            };
        }
        if runtime.starts_with("docker://") || (runtime.starts_with("containerd://") && nerdctl) {
            return Ok(Self::Shared);
        }
        if runtime.starts_with("containerd://")
            && (provider.starts_with("k3s://")
                || info["kubeletVersion"]
                    .as_str()
                    .unwrap_or("")
                    .contains("+k3s"))
        {
            return Ok(Self::K3s);
        }
        Err((
            FloorUnarmed::ImageUnavailable,
            format!("unsupported node image store ({runtime:?}); {UNKNOWN_FIX}"),
        ))
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Kind { .. } => "kind load",
            Self::K3s => "k3s import",
        }
    }

    pub(crate) fn fix(&self) -> String {
        match self {
            Self::Shared => "rebuild the image in the node's shared store; select nerdctl on \
                the node's containerd when using containerd (namespace k8s.io)"
                .into(),
            Self::Kind { cluster } => format!(
                "install kind on the daemon's PATH and make \
                 `kind load image-archive <archive> --name {cluster}` work for this cluster"
            ),
            Self::K3s => "install k3s on the daemon's PATH; the daemon needs access to the k3s \
                containerd socket for `k3s ctr --namespace k8s.io images import <archive>`, or \
                select nerdctl with CONTAINERD_ADDRESS=/run/k3s/containerd/containerd.sock"
                .into(),
        }
    }
}

/// A private, uniquely-created directory. All archives disappear on success and refusal.
pub(crate) fn archive_dir() -> Result<RemoveOnDrop, String> {
    let dir = std::env::temp_dir().join(format!("wicked-image-{}", uuid::Uuid::new_v4().simple()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&dir)
        .map_err(|e| format!("cannot create image archive directory: {e}"))?;
    Ok(RemoveOnDrop(dir))
}

/// Serialize imports and remember only successful loads, once per cluster/node incarnation/id.
pub(crate) fn ensure_loaded(
    kube: &Kube,
    rt: &Runtime,
    node: &NodeInfo,
    tags: &[&str],
) -> Result<(), (FloorUnarmed, String)> {
    if node.store == ImageStore::Shared {
        return Ok(());
    }
    static LOADED: Mutex<Option<HashSet<(Vec<String>, String, String)>>> = Mutex::new(None);
    let refuse = |e: String| {
        (
            FloorUnarmed::ImageUnavailable,
            format!("{e}; {}", node.store.fix()),
        )
    };
    let mut cache = LOADED
        .lock()
        .map_err(|_| refuse("image load lock is poisoned".into()))?;
    let loaded = cache.get_or_insert_with(HashSet::new);
    for tag in tags {
        let id = image_id(rt, tag)
            .ok_or_else(|| refuse(format!("cannot inspect image {tag} before loading it")))?;
        let key = (kube.head(None), node.uid.clone(), id);
        if loaded.contains(&key) {
            continue;
        }
        let dir = archive_dir().map_err(&refuse)?;
        let path = dir.0.join("image.tar");
        let path_arg = path.to_string_lossy().into_owned();
        let mut save = rt.cmd();
        save.args(["save", "-o", &path_arg, tag]);
        let r = super::run_bounded(save, None, rt.bound(IMAGE_TRANSFER_TIMEOUT));
        if !r.ok() {
            return Err(refuse(format!(
                "image save -o {path_arg} {tag} failed: {}",
                r.err_text()
            )));
        }
        std::fs::File::open(&path)
            .map_err(|e| refuse(format!("cannot read saved image archive: {e}")))?;
        let (binary, args): (&str, Vec<&str>) = match &node.store {
            ImageStore::Kind { cluster } => (
                "kind",
                vec!["load", "image-archive", &path_arg, "--name", cluster],
            ),
            ImageStore::K3s => (
                "k3s",
                vec![
                    "ctr",
                    "--namespace",
                    "k8s.io",
                    "images",
                    "import",
                    &path_arg,
                ],
            ),
            ImageStore::Shared => unreachable!(),
        };
        let local = rt
            .cli
            .parent()
            .map(|d| d.join(binary))
            .filter(|p| p.is_file());
        let executable = local
            .or_else(|| crate::validator::find_on_path(binary))
            .ok_or_else(|| refuse(format!("{binary} image loader was not found")))?;
        let mut load = Command::new(executable);
        load.hardened().stdin(Stdio::null()).args(&args);
        let r = super::run_bounded(load, None, rt.bound(IMAGE_TRANSFER_TIMEOUT));
        if !r.ok() {
            return Err(refuse(format!(
                "{binary} {} failed: {}",
                args.join(" "),
                r.err_text()
            )));
        }
        loaded.insert(key);
    }
    Ok(())
}

/// Resolve the node anew before an arm, including when its image loads are already memoized.
pub(crate) fn ensure_for(
    kube: &Kube,
    rt: &Runtime,
    tags: &[&str],
) -> Result<(), (FloorUnarmed, String)> {
    let node = super::cluster::single_node(kube, rt)?;
    ensure_loaded(kube, rt, &node, tags)
}

/// Used only on an image-start refusal; failed classification keeps its own named fix.
pub(crate) fn start_fix(kube: &Kube) -> String {
    match crate::container_floor::runtime()
        .map_err(|w| (w, w.describe().to_string()))
        .and_then(|rt| super::cluster::single_node(kube, &rt))
    {
        Ok(node) => node.store.fix(),
        Err((_, e)) => format!("{e}; {UNKNOWN_FIX}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_store_classification_is_explicit() {
        for (provider, runtime, kubelet, nerdctl, expected) in [
            (
                "kind://docker/c/c-control-plane",
                "containerd://1.7",
                "v1.31",
                false,
                Some(ImageStore::Kind {
                    cluster: "c".into(),
                }),
            ),
            (
                "",
                "docker://27.0.0",
                "v1.31",
                false,
                Some(ImageStore::Shared),
            ),
            (
                "",
                "containerd://1.7",
                "v1.31",
                true,
                Some(ImageStore::Shared),
            ),
            (
                "k3s://node",
                "containerd://1.7",
                "v1.31",
                false,
                Some(ImageStore::K3s),
            ),
            (
                "",
                "containerd://1.7",
                "v1.31+k3s1",
                false,
                Some(ImageStore::K3s),
            ),
            ("", "containerd://1.7", "v1.31", false, None),
            ("", "", "", false, None),
            ("", "cri-o://1.31", "v1.31", false, None),
            ("kind://broken", "docker://27", "v1.31", false, None),
        ] {
            let n = json!({"spec": {"providerID": provider}, "status": {"nodeInfo": {
                "containerRuntimeVersion": runtime, "kubeletVersion": kubelet
            }}});
            let got = ImageStore::classify(&n, nerdctl);
            match expected {
                Some(want) => assert_eq!(got.unwrap(), want),
                None => {
                    let (why, fix) = got.unwrap_err();
                    assert_eq!(why, FloorUnarmed::ImageUnavailable);
                    assert!(fix.contains("select nerdctl"), "{fix}");
                }
            }
        }
    }

    #[cfg(unix)]
    fn fixture(
        store: ImageStore,
        fail: bool,
    ) -> (
        crate::container_floor::RemoveOnDrop,
        Kube,
        Runtime,
        NodeInfo,
        std::path::PathBuf,
    ) {
        let dir = std::env::temp_dir().join(format!("wicked-images-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls");
        let cli = dir.join("docker");
        crate::test_env::install_stub(&cli, &format!(
            "#!/bin/sh\necho \"docker $*\" >> '{}'\ncase \"$1\" in\nimage) echo sha256:{};;\nsave) : > \"$3\";;\n*) exit 99;;\nesac\n", log.display(), "a".repeat(64)));
        for name in ["kind", "k3s"] {
            crate::test_env::install_stub(
                &dir.join(name),
                &format!(
                    "#!/bin/sh\necho \"{name} $*\" >> '{}'\n{}\n",
                    log.display(),
                    if fail {
                        "echo load-broke >&2; exit 3"
                    } else {
                        "exit 0"
                    }
                ),
            );
        }
        let kubectl = dir.join("kubectl");
        crate::test_env::install_stub(&kubectl, &format!(
            "#!/bin/sh\necho \"kubectl $*\" >> '{}'\ncase \"$*\" in\n*'get nodes -o json'*) echo '{{\"items\":[{{\"metadata\":{{\"name\":\"n\",\"uid\":\"u\"}},\"status\":{{\"nodeInfo\":{{\"containerRuntimeVersion\":\"containerd://1.7\"}}}}}}]}}';;\n*) exit 99;;\nesac\n", log.display()));
        let kube = Kube {
            kubectl,
            kubeconfig: None,
            context: dir.display().to_string(),
            owner: "wtest".into(),
        };
        let node = NodeInfo {
            name: "n".into(),
            uid: dir.display().to_string(),
            store,
        };
        (
            crate::container_floor::RemoveOnDrop(dir),
            kube,
            Runtime::answering(cli),
            node,
            log,
        )
    }

    #[cfg(unix)]
    #[test]
    fn kind_loads_once_per_node_and_image_and_cleans_the_archive() {
        let (_dir, kube, rt, node, log) = fixture(
            ImageStore::Kind {
                cluster: "c".into(),
            },
            false,
        );
        for _ in 0..2 {
            ensure_loaded(&kube, &rt, &node, &["seat:stable"]).unwrap();
        }
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.matches("docker save -o ").count(), 1, "{calls}");
        assert_eq!(
            calls.matches("kind load image-archive ").count(),
            1,
            "{calls}"
        );
        assert!(calls.contains("--name c"), "{calls}");
        let save = calls
            .lines()
            .find(|l| l.starts_with("docker save"))
            .unwrap();
        assert!(!std::path::Path::new(save.split_whitespace().nth(3).unwrap()).exists());
        let mut replacement = node.clone();
        replacement.uid.push_str("-new");
        ensure_loaded(&kube, &rt, &replacement, &["seat:stable"]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&log)
                .unwrap()
                .matches("kind load")
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn shared_store_runs_no_load_or_save() {
        let (_dir, kube, rt, node, log) = fixture(ImageStore::Shared, false);
        ensure_loaded(&kube, &rt, &node, &["seat:stable"]).unwrap();
        assert!(!log.exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_kind_load_refuses_with_the_command_and_fix_and_is_retried() {
        let (_dir, kube, rt, node, log) = fixture(
            ImageStore::Kind {
                cluster: "c".into(),
            },
            true,
        );
        for _ in 0..2 {
            let (why, text) = ensure_loaded(&kube, &rt, &node, &["seat:stable"]).unwrap_err();
            assert_eq!(why, FloorUnarmed::ImageUnavailable);
            assert!(
                text.contains("load-broke") && text.contains("kind load image-archive"),
                "{text}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .matches("kind load")
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn k3s_import_uses_the_kubelet_namespace_and_names_the_socket_fix() {
        let (_dir, kube, rt, node, log) = fixture(ImageStore::K3s, true);
        let (why, text) = ensure_loaded(&kube, &rt, &node, &["seat:x"]).unwrap_err();
        assert_eq!(why, FloorUnarmed::ImageUnavailable);
        assert!(
            text.contains("CONTAINERD_ADDRESS=/run/k3s/containerd/containerd.sock"),
            "{text}"
        );
        assert!(std::fs::read_to_string(log)
            .unwrap()
            .contains("k3s ctr --namespace k8s.io images import"));
    }

    #[cfg(unix)]
    #[test]
    fn unknown_containerd_refuses_before_a_pod_is_applied() {
        let (_dir, kube, rt, _node, log) = fixture(ImageStore::Shared, false);
        let (why, text) = super::super::cluster::single_node(&kube, &rt).unwrap_err();
        assert_eq!(why, FloorUnarmed::ImageUnavailable);
        assert!(text.contains("select nerdctl"));
        assert!(!std::fs::read_to_string(log).unwrap().contains("apply"));
    }
}
