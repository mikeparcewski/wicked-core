//! Image holders: the seat images survive the container runtime's image garbage collection.
//!
//! A desktop runtime that also runs Kubernetes (Rancher Desktop's k3s) has a kubelet that deletes
//! every image no container references once its image disk passes `imageGCHighThresholdPercent`
//! (85 by default), down to the low threshold. The engine's seat, floor and checks images are
//! referenced only while a seat or a floor runs (each is a `--rm` container), so between turns they
//! are the first to go, and the rig's own seat base (`WICKED_SEAT_BASE_IMAGE`) cannot be pulled
//! back. The runtime refuses to delete an image a container still uses (`conflict: unable to delete
//! … (must be forced) - container … is using its referenced image`), and the kubelet never forces.
//!
//! So the engine keeps one stopped, never-started HOLDER container per image it needs
//! (`wicked-keep-…`, label [`KEEP_LABEL`]`=1`). A holder is an empty created container: it costs a
//! few kilobytes, runs nothing (its entrypoint does not exist) and has no network. The seat sweep's
//! `wicked.seat=1` filter never takes one: a holder overrides the label it would inherit from a
//! seat image. When a tag moves to a new image, its old holder goes, so the old image is the
//! runtime's to reclaim again; the newest [`KEEP_PER_REPO`] tags of each repository stay held, so
//! two daemons on different engine builds (one VM) each keep their own seat images.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::container_floor::{output_within, Runtime};

/// The label on every holder container.
pub(crate) const KEEP_LABEL: &str = "wicked.keep";
/// The image reference a holder holds (`wicked-seat-claude:d3712b4dbefa`).
const KEEP_IMAGE_LABEL: &str = "wicked.keep.image";
/// The image id it holds (`sha256:…`).
const KEEP_ID_LABEL: &str = "wicked.keep.id";
/// When it was made (unix seconds), for the per-repository cap.
const KEEP_AT_LABEL: &str = "wicked.keep.at";
/// Holder names start with this.
pub(crate) const KEEP_PREFIX: &str = "wicked-keep-";
/// Tags held per image repository: the current one and one more (a second daemon on another
/// engine build shares the runtime).
pub(crate) const KEEP_PER_REPO: usize = 2;
/// How long a reference held by this process is trusted before it is checked again (an operator's
/// `docker container prune` removes holders; the next check puts them back).
const KEEP_EVERY: Duration = Duration::from_secs(10 * 60);
const PROBE: Duration = Duration::from_secs(15);

/// One holder container, as `ps` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Holder {
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) id: String,
    pub(crate) at: u64,
}

/// What [`plan`] decided: holders to make (`(reference, image id)`) and holders to remove (names).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) create: Vec<(String, String)>,
    pub(crate) remove: Vec<String>,
}

/// The repository of an image reference: everything before the tag (a `:` after the last `/`),
/// without a digest.
pub(crate) fn repository(image: &str) -> &str {
    let image = image.split('@').next().unwrap_or(image);
    let slash = image.rfind('/').map_or(0, |i| i + 1);
    match image[slash..].rfind(':') {
        Some(i) => &image[..slash + i],
        None => image,
    }
}

/// The holder's container name: [`KEEP_PREFIX`], the reference as a name, the id's first 12 hex.
pub(crate) fn holder_name(image: &str, id: &str) -> String {
    let mut slug: String = image
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug.truncate(80);
    let hex: String = id
        .strip_prefix("sha256:")
        .unwrap_or(id)
        .chars()
        .take(12)
        .collect();
    format!("{KEEP_PREFIX}{}-{hex}", slug.trim_matches('-'))
}

/// The runtime's argv that makes the holder of `image` (id `id`), made at `at`: created, never
/// started, no network, an entrypoint that does not exist, never pulled.
pub(crate) fn create_argv(image: &str, id: &str, at: u64) -> Vec<String> {
    let mut v: Vec<String> = vec!["create".into(), "--name".into(), holder_name(image, id)];
    for l in [
        format!("{KEEP_LABEL}=1"),
        format!("{KEEP_IMAGE_LABEL}={image}"),
        format!("{KEEP_ID_LABEL}={id}"),
        format!("{KEEP_AT_LABEL}={at}"),
        // A seat image carries `wicked.seat=1`, which a container inherits: overridden, so the
        // seat sweep (`container prune --filter label=wicked.seat=1`) never removes a holder.
        format!("{}=keep", crate::container_floor::SEAT_LABEL),
    ] {
        v.push("--label".into());
        v.push(l);
    }
    v.extend(
        [
            "--network",
            "none",
            "--pull",
            "never",
            "--entrypoint",
            "/wicked-keep",
            id,
        ]
        .map(String::from),
    );
    v
}

/// The runtime's argv that lists the holders.
pub(crate) fn list_argv() -> Vec<String> {
    vec![
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        format!("label={KEEP_LABEL}=1"),
        "--format".into(),
        format!(
            "{{{{.Names}}}}\t{{{{.Label \"{KEEP_IMAGE_LABEL}\"}}}}\t{{{{.Label \"{KEEP_ID_LABEL}\"}}}}\t{{{{.Label \"{KEEP_AT_LABEL}\"}}}}"
        ),
    ]
}

/// Parse [`list_argv`]'s output. A line without a name or an image is not a holder of ours.
pub(crate) fn parse_list(out: &str) -> Vec<Holder> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let name = f.next()?.trim().to_string();
            let image = f.next()?.trim().to_string();
            if name.is_empty() || image.is_empty() || !name.starts_with(KEEP_PREFIX) {
                return None;
            }
            let id = f.next().unwrap_or("").trim().to_string();
            let at = f.next().and_then(|a| a.trim().parse().ok()).unwrap_or(0);
            Some(Holder {
                name,
                image,
                id,
                at,
            })
        })
        .collect()
}

/// Decide the holders: one for each wanted `(reference, image id)` that has none; remove a holder
/// whose reference now names another image (the tag moved); per repository keep the newest
/// [`KEEP_PER_REPO`] references held (a wanted one always stays) and remove the older ones.
pub(crate) fn plan(holders: &[Holder], wanted: &[(String, String)]) -> Plan {
    let mut out = Plan::default();
    let current: HashMap<&str, &str> = wanted
        .iter()
        .map(|(r, i)| (r.as_str(), i.as_str()))
        .collect();
    for (r, i) in wanted {
        if !holders.iter().any(|h| &h.image == r && &h.id == i)
            && !out.create.iter().any(|(cr, ci)| cr == r && ci == i)
        {
            out.create.push((r.clone(), i.clone()));
        }
    }
    let mut kept: Vec<&Holder> = Vec::new();
    for h in holders {
        let moved = current
            .get(h.image.as_str())
            .is_some_and(|i| *i != h.id.as_str());
        if moved {
            out.remove.push(h.name.clone());
        } else {
            kept.push(h);
        }
    }
    // Per repository: the newest reference first (a wanted reference counts as newest).
    let mut by_repo: BTreeMap<&str, Vec<(&str, u64)>> = BTreeMap::new();
    for h in &kept {
        let at = if current.contains_key(h.image.as_str()) {
            u64::MAX
        } else {
            h.at
        };
        let refs = by_repo.entry(repository(&h.image)).or_default();
        match refs.iter_mut().find(|(r, _)| *r == h.image.as_str()) {
            Some(e) => e.1 = e.1.max(at),
            None => refs.push((h.image.as_str(), at)),
        }
    }
    for (r, _) in wanted {
        let refs = by_repo.entry(repository(r)).or_default();
        if !refs.iter().any(|(x, _)| x == r) {
            refs.push((r.as_str(), u64::MAX));
        }
    }
    for refs in by_repo.values_mut() {
        refs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        for (old, _) in refs.iter().skip(KEEP_PER_REPO) {
            if current.contains_key(old) {
                continue;
            }
            for h in kept.iter().filter(|h| h.image == *old) {
                out.remove.push(h.name.clone());
            }
        }
    }
    out.remove.sort();
    out.remove.dedup();
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// References this process held, and when it last checked them.
fn checked() -> &'static Mutex<HashMap<String, Instant>> {
    static CHECKED: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> =
        std::sync::OnceLock::new();
    CHECKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Hold each of `images` (references present on the runtime) with a holder container. Best effort:
/// a failure is logged and returned, never a reason to refuse a unit (the image is there now; a
/// holder only keeps it there). Checks each reference at most once per [`KEEP_EVERY`] per process.
pub(crate) fn hold(rt: &Runtime, images: &[&str]) -> Result<(), String> {
    let due: Vec<&str> = {
        let g = checked().lock().unwrap_or_else(|p| p.into_inner());
        images
            .iter()
            .copied()
            .filter(|i| !i.is_empty())
            .filter(|i| g.get(*i).is_none_or(|t| t.elapsed() >= KEEP_EVERY))
            .collect()
    };
    if due.is_empty() {
        return Ok(());
    }
    let r = hold_now(rt, &due);
    if let Err(e) = &r {
        eprintln!("wicked-core: could not hold the seat images against image cleanup: {e}");
    }
    r
}

/// [`hold`] without the per-process memo (the `seat-image` CLI, and a due check).
pub(crate) fn hold_now(rt: &Runtime, images: &[&str]) -> Result<(), String> {
    let mut wanted: Vec<(String, String)> = Vec::new();
    for i in images {
        if let Some(id) = crate::container_floor::image_id_of(rt, i) {
            wanted.push((i.to_string(), id));
        }
    }
    let holders = list(rt)?;
    let p = plan(&holders, &wanted);
    let mut errs: Vec<String> = Vec::new();
    for name in &p.remove {
        let mut c = rt.cmd();
        c.args(["rm", name.as_str()]);
        match output_within(c, rt.bound(PROBE)) {
            Some(o) if o.status.success() => {}
            Some(o) => errs.push(format!(
                "removing {name}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            None => errs.push(format!("removing {name} timed out")),
        }
    }
    let at = now_secs();
    let mut made: Vec<&str> = Vec::new();
    for (image, id) in &p.create {
        let mut c = rt.cmd();
        c.args(create_argv(image, id, at));
        match output_within(c, rt.bound(PROBE)) {
            Some(o) if o.status.success() => made.push(image),
            // Another engine process made the same holder first.
            Some(o) if String::from_utf8_lossy(&o.stderr).contains("already in use") => {
                made.push(image)
            }
            Some(o) => errs.push(format!(
                "holding {image}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            None => errs.push(format!("holding {image} timed out")),
        }
    }
    {
        let mut g = checked().lock().unwrap_or_else(|p| p.into_inner());
        for (image, _) in &wanted {
            let ok = !p.create.iter().any(|(i, _)| i == image) || made.contains(&image.as_str());
            if ok {
                g.insert(image.clone(), Instant::now());
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

/// The holders on the runtime now.
pub(crate) fn list(rt: &Runtime) -> Result<Vec<Holder>, String> {
    let mut c = rt.cmd();
    c.args(list_argv());
    match output_within(c, rt.bound(PROBE)) {
        Some(o) if o.status.success() => Ok(parse_list(&String::from_utf8_lossy(&o.stdout))),
        Some(o) => Err(format!(
            "listing holders: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        None => Err("listing holders timed out".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, image: &str, id: &str, at: u64) -> Holder {
        Holder {
            name: name.into(),
            image: image.into(),
            id: id.into(),
            at,
        }
    }

    fn w(image: &str, id: &str) -> (String, String) {
        (image.into(), id.into())
    }

    #[test]
    fn repository_strips_the_tag_and_digest_but_not_a_registry_port() {
        assert_eq!(
            repository("wicked-seat-claude:d3712b4dbefa"),
            "wicked-seat-claude"
        );
        assert_eq!(repository("localhost:5000/seat:1"), "localhost:5000/seat");
        assert_eq!(repository("localhost:5000/seat"), "localhost:5000/seat");
        assert_eq!(repository("node@sha256:abc"), "node");
        assert_eq!(repository("busybox"), "busybox");
    }

    #[test]
    fn a_holder_is_created_never_started_offline_and_escapes_the_seat_sweep() {
        let v = create_argv("wicked-seat-base:node22-rust", "sha256:56a12663c21bffff", 7);
        assert_eq!(v[0], "create");
        assert_eq!(
            v[2],
            "wicked-keep-wicked-seat-base-node22-rust-56a12663c21b"
        );
        for l in [
            "wicked.keep=1",
            "wicked.keep.image=wicked-seat-base:node22-rust",
            "wicked.keep.id=sha256:56a12663c21bffff",
            "wicked.keep.at=7",
            // The sweep prunes label=wicked.seat=1: a holder must not match it.
            "wicked.seat=keep",
        ] {
            assert!(v.windows(2).any(|p| p[0] == "--label" && p[1] == l), "{l}");
        }
        assert!(v.windows(2).any(|p| p[0] == "--network" && p[1] == "none"));
        assert!(v.windows(2).any(|p| p[0] == "--pull" && p[1] == "never"));
        assert!(v
            .windows(2)
            .any(|p| p[0] == "--entrypoint" && p[1] == "/wicked-keep"));
        assert_eq!(v.last().unwrap(), "sha256:56a12663c21bffff");
        assert!(!v.iter().any(|a| a == "run" || a == "start"));
        let sweep = crate::container_floor::sweep_argv(Duration::from_secs(3600));
        assert!(sweep[0].contains(&"label=wicked.seat=1".to_string()));
    }

    #[test]
    fn the_list_parses_holders_and_skips_foreign_lines() {
        let out = "wicked-keep-a-1\twicked-seat-claude:1\tsha256:1\t10\n\
                   other\tx:1\tsha256:2\t3\n\
                   wicked-keep-old\twicked-seat-base:node22-rust\t\t\n";
        assert_eq!(
            parse_list(out),
            vec![
                h("wicked-keep-a-1", "wicked-seat-claude:1", "sha256:1", 10),
                h("wicked-keep-old", "wicked-seat-base:node22-rust", "", 0),
            ]
        );
    }

    #[test]
    fn a_missing_holder_is_made_and_a_present_one_left_alone() {
        let hs = [h("k1", "wicked-seat-claude:1", "sha256:a", 5)];
        let p = plan(
            &hs,
            &[
                w("wicked-seat-claude:1", "sha256:a"),
                w("wicked-floor:9", "sha256:f"),
            ],
        );
        assert_eq!(p.create, vec![w("wicked-floor:9", "sha256:f")]);
        assert!(p.remove.is_empty());
    }

    #[test]
    fn a_moved_tag_releases_its_old_image() {
        // The rig's base was rebuilt under the same tag: the old image is no longer needed.
        let hs = [h("k-old", "wicked-seat-base:node22-rust", "sha256:old", 5)];
        let p = plan(&hs, &[w("wicked-seat-base:node22-rust", "sha256:new")]);
        assert_eq!(
            p.create,
            vec![w("wicked-seat-base:node22-rust", "sha256:new")]
        );
        assert_eq!(p.remove, vec!["k-old".to_string()]);
    }

    #[test]
    fn each_repository_keeps_its_newest_two_tags_and_always_the_wanted_one() {
        let hs = [
            h("k-1", "wicked-seat-claude:1", "sha256:1", 100),
            h("k-2", "wicked-seat-claude:2", "sha256:2", 200),
            h("k-3", "wicked-seat-claude:3", "sha256:3", 300),
            h("k-c", "wicked-seat-codex:1", "sha256:c", 50),
        ];
        // A daemon on build 4 holds its tag: 4 and the newest other (3) stay.
        let p = plan(&hs, &[w("wicked-seat-claude:4", "sha256:4")]);
        assert_eq!(p.create, vec![w("wicked-seat-claude:4", "sha256:4")]);
        assert_eq!(p.remove, vec!["k-1".to_string(), "k-2".to_string()]);
        // A daemon still on build 1 keeps its own tag even though it is the oldest.
        let p = plan(&hs, &[w("wicked-seat-claude:1", "sha256:1")]);
        assert!(p.create.is_empty());
        assert_eq!(p.remove, vec!["k-2".to_string()]);
    }

    #[test]
    fn hold_makes_the_missing_holder_through_the_runtime() {
        let dir = std::env::temp_dir().join(format!("wk-keep-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = dir.join("runtime");
        let log = dir.join("calls.log");
        crate::test_env::install_stub(
            &rt,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n\
                 case \"$1 $2\" in\n\
                 'image inspect') echo sha256:{} ;;\n\
                 'ps -a') printf 'wicked-keep-stale\\tseat-x:1\\tsha256:old\\t1\\n' ;;\n\
                 esac\nexit 0\n",
                log.display(),
                "a".repeat(64)
            ),
        );
        let rt = Runtime::answering(rt);
        hold_now(&rt, &["seat-x:1"]).unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("rm wicked-keep-stale"), "{calls}");
        assert!(
            calls.contains(&format!(
                "create --name wicked-keep-seat-x-1-{} --label wicked.keep=1",
                "a".repeat(12)
            )),
            "{calls}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
