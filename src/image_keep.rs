//! Image holders: the seat images survive the container runtime's image garbage collection.
//!
//! A desktop runtime that also runs Kubernetes (Rancher Desktop's k3s on the moby engine, kubelet
//! through cri-dockerd) deletes every image no container references once its image disk passes
//! `imageGCHighThresholdPercent` (85 by default), down to the low threshold. The engine's seat,
//! floor and checks images are referenced only while a seat or a floor runs (each is a `--rm`
//! container), so between turns they are the first to go, and the rig's own seat base
//! (`WICKED_SEAT_BASE_IMAGE`) cannot be pulled back. dockerd refuses to delete an image a container
//! still uses, even a stopped one (`conflict: unable to delete … (must be forced) - container … is
//! using its referenced image`), and cri-dockerd never forces.
//!
//! So the engine keeps a stopped, never-started HOLDER container per image it needs (`wicked-keep-…`,
//! label [`KEEP_LABEL`]`=1`): created with no network, never pulled, and an entrypoint that is a
//! directory (`/`), so it cannot run even if someone starts it. An image that declares a `VOLUME`
//! is not held (creating the holder would allocate and fill the volume). The seat sweep's
//! `wicked.seat=1` filter never takes a holder: it overrides the label it would inherit.
//!
//! Holders are leases, renewed and retired by age, never by guessing who still needs an image: a
//! process that holds an image renews its holder once it is older than [`RENEW_AFTER`] (a new
//! holder first, then the old one goes); a holder nobody renewed for [`STALE_AFTER`] is retired by
//! the next process that holds anything (a moved tag's old image, a build no daemon runs any more).
//! Two daemons on different engine builds sharing one runtime each keep their own images held.
//!
//! Only the docker CLI (moby) is held: a containerd runtime's CRI image removal does not check
//! stopped nerdctl containers, and podman runs no kubelet; there the status says not supported.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::container_floor::{output_within, Runtime};

/// The label on every holder container.
pub(crate) const KEEP_LABEL: &str = "wicked.keep";
/// The image reference a holder holds (`wicked-seat-claude:d3712b4dbefa`).
const KEEP_IMAGE_LABEL: &str = "wicked.keep.image";
/// The image id it holds (`sha256:…`).
const KEEP_ID_LABEL: &str = "wicked.keep.id";
/// When it was made (unix seconds): its lease.
const KEEP_AT_LABEL: &str = "wicked.keep.at";
/// Holder names start with this.
pub(crate) const KEEP_PREFIX: &str = "wicked-keep-";
/// A holder still wanted is renewed once it is this old.
pub(crate) const RENEW_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
/// A holder nobody renewed for this long is retired.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(3 * 24 * 60 * 60);
/// How long a reference held by this process is trusted before it is checked again (an operator's
/// `docker container prune` removes holders; the next check puts them back). A build always holds
/// at once.
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

/// What [`plan`] decided.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Holders to make: `(reference, image id)`.
    pub(crate) create: Vec<(String, String)>,
    /// Holders a new one replaces: removed only once that reference's new holder exists.
    pub(crate) renewed: Vec<(String, String)>,
    /// Holders nobody renewed for [`STALE_AFTER`]: removed now.
    pub(crate) retire: Vec<String>,
}

/// Whether this runtime's holders protect its images (see the module doc).
pub(crate) fn supported(rt: &Runtime) -> bool {
    rt.cli
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("docker"))
}

fn bare(id: &str) -> &str {
    id.strip_prefix("sha256:").unwrap_or(id)
}

/// 12 hex of sha256 over `s`.
fn hash12(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The holder's container name: [`KEEP_PREFIX`], the reference as a readable name, a hash of the
/// whole reference (two references the slug spells alike stay apart), the id's first 12 hex, and
/// the lease time.
pub(crate) fn holder_name(image: &str, id: &str, at: u64) -> String {
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
    slug.truncate(48);
    let hex: String = bare(id).chars().take(12).collect();
    format!(
        "{KEEP_PREFIX}{}-{}-{hex}-{at}",
        slug.trim_matches('-'),
        hash12(image)
    )
}

/// The runtime's argv that makes the holder of `image` (id `id`) at `at`: created, never started,
/// no network, never pulled, a directory for an entrypoint.
pub(crate) fn create_argv(image: &str, id: &str, at: u64) -> Vec<String> {
    let mut v: Vec<String> = vec!["create".into(), "--name".into(), holder_name(image, id, at)];
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
            // A directory: exec fails even if the holder is ever started.
            "--entrypoint",
            "/",
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

/// Parse [`list_argv`]'s output. A line without a name, an image, or our prefix is not a holder.
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

/// Decide the holders at `now` (unix seconds): one for each wanted `(reference, image id)` that has
/// none, or whose newest holder is past [`RENEW_AFTER`] (renewed: its old holders go once the new
/// one exists); every holder of anything not wanted here whose lease is past [`STALE_AFTER`] is
/// retired. Nothing is removed for being "another" tag: a daemon that still uses it renews it.
pub(crate) fn plan(holders: &[Holder], wanted: &[(String, String)], now: u64) -> Plan {
    let mut out = Plan::default();
    let age = |h: &Holder| now.saturating_sub(h.at);
    let is_wanted = |h: &Holder| {
        wanted
            .iter()
            .any(|(r, i)| &h.image == r && bare(&h.id) == bare(i))
    };
    for (r, i) in wanted {
        if out.create.iter().any(|(cr, ci)| cr == r && ci == i) {
            continue;
        }
        let mine: Vec<&Holder> = holders
            .iter()
            .filter(|h| &h.image == r && bare(&h.id) == bare(i))
            .collect();
        let fresh = mine.iter().any(|h| age(h) < RENEW_AFTER.as_secs());
        if !fresh {
            out.create.push((r.clone(), i.clone()));
            for h in mine {
                out.renewed.push((r.clone(), h.name.clone()));
            }
        }
    }
    for h in holders {
        if !is_wanted(h) && age(h) >= STALE_AFTER.as_secs() {
            out.retire.push(h.name.clone());
        }
    }
    out.retire.sort();
    out.retire.dedup();
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// References this process held (per runtime), and when it last checked them.
fn checked() -> &'static Mutex<HashMap<String, Instant>> {
    static CHECKED: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> =
        std::sync::OnceLock::new();
    CHECKED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn memo_key(rt: &Runtime, image: &str) -> String {
    format!("{}\u{0}{image}", rt.cli.display())
}

/// Hold each of `images` (references present on the runtime) with a holder container, checking
/// each at most once per [`KEEP_EVERY`] per process. Best effort: a failure is logged and returned,
/// never a reason to refuse a unit (the image is there now; a holder only keeps it there).
pub(crate) fn hold(rt: &Runtime, images: &[&str]) -> Result<(), String> {
    if !supported(rt) {
        return Ok(());
    }
    let due: Vec<&str> = {
        let g = checked().lock().unwrap_or_else(|p| p.into_inner());
        images
            .iter()
            .copied()
            .filter(|i| !i.is_empty())
            .filter(|i| {
                g.get(&memo_key(rt, i))
                    .is_none_or(|t| t.elapsed() >= KEEP_EVERY)
            })
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

/// [`hold`] without the per-process memo: right after a build (the image is new, whatever the memo
/// says), and the `seat-image` CLI.
pub(crate) fn hold_now(rt: &Runtime, images: &[&str]) -> Result<(), String> {
    if !supported(rt) {
        return Ok(());
    }
    hold_unmemoized(rt, images)
}

fn run(rt: &Runtime, args: &[String]) -> Result<String, String> {
    let mut c = rt.cmd();
    c.args(args);
    match output_within(c, rt.bound(PROBE)) {
        Some(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Some(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        None => Err(format!(
            "`{}` timed out",
            args.first().map(String::as_str).unwrap_or("")
        )),
    }
}

/// Remove a holder and the anonymous volumes it may have (`rm -v`); gone already is fine.
fn remove(rt: &Runtime, name: &str) -> Result<(), String> {
    match run(rt, &["rm".into(), "-v".into(), name.into()]) {
        Ok(_) => Ok(()),
        Err(e) if e.contains("No such container") => Ok(()),
        Err(e) => Err(format!("removing {name}: {e}")),
    }
}

/// The image's declared volumes (`image inspect … .Config.Volumes`), empty when none.
fn declares_volumes(rt: &Runtime, id: &str) -> Result<bool, String> {
    let out = run(
        rt,
        &[
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{json .Config.Volumes}}".into(),
            id.into(),
        ],
    )?;
    let v = out.trim();
    Ok(!(v.is_empty() || v == "null" || v == "{}"))
}

/// Whether the container `name` is our holder of `(image, id)`.
fn is_holder_of(rt: &Runtime, name: &str, image: &str, id: &str) -> bool {
    run(
        rt,
        &[
            "container".into(),
            "inspect".into(),
            "--format".into(),
            format!(
                "{{{{index .Config.Labels \"{KEEP_LABEL}\"}}}}\t{{{{index .Config.Labels \"{KEEP_IMAGE_LABEL}\"}}}}\t{{{{.Image}}}}"
            ),
            name.into(),
        ],
    )
    .is_ok_and(|o| {
        let f: Vec<&str> = o.trim().split('\t').collect();
        f.len() == 3 && f[0] == "1" && f[1] == image && bare(f[2]) == bare(id)
    })
}

fn hold_unmemoized(rt: &Runtime, images: &[&str]) -> Result<(), String> {
    let mut wanted: Vec<(String, String)> = Vec::new();
    for i in images {
        if let Some(id) = crate::container_floor::image_id_of(rt, i) {
            wanted.push((i.to_string(), id));
        }
    }
    let memo_all = |except: &[&str]| {
        let mut g = checked().lock().unwrap_or_else(|p| p.into_inner());
        for i in images
            .iter()
            .filter(|i| !i.is_empty() && !except.contains(i))
        {
            g.insert(memo_key(rt, i), Instant::now());
        }
    };
    if wanted.is_empty() {
        // Nothing here to hold (an image that is not there): checked, so a dispatch does not
        // re-ask the runtime before the memo runs out; a build holds at once regardless.
        memo_all(&[]);
        return Ok(());
    }
    let holders = parse_list(&run(rt, &list_argv()).map_err(|e| format!("listing holders: {e}"))?);
    let now = now_secs();
    let p = plan(&holders, &wanted, now);
    let mut errs: Vec<String> = Vec::new();
    let mut held: Vec<&str> = Vec::new();
    for (image, id) in &p.create {
        // The tag may have moved since it was read (another engine rebuilt it): this engine holds
        // nothing for it and leaves the other engine's holder alone; said, so a caller retries.
        if crate::container_floor::image_id_of(rt, image)
            .is_none_or(|now_id| bare(&now_id) != bare(id))
        {
            errs.push(format!(
                "{image} changed while it was being held; not held, check again"
            ));
            continue;
        }
        match declares_volumes(rt, id) {
            Ok(false) => {}
            Ok(true) => {
                errs.push(format!(
                    "{image} declares a volume: not held (a holder would allocate it)"
                ));
                continue;
            }
            Err(e) => {
                errs.push(format!("holding {image}: {e}"));
                continue;
            }
        }
        let argv = create_argv(image, id, now);
        match run(rt, &argv) {
            Ok(_) => held.push(image),
            // Another engine process made the same holder in the same second: accepted only when
            // the container by that name is our holder of this very image.
            Err(e)
                if (e.contains("already in use") || e.contains("already used"))
                    && is_holder_of(rt, &argv[2], image, id) =>
            {
                held.push(image)
            }
            Err(e) => errs.push(format!("holding {image}: {e}")),
        }
    }
    // A renewed holder's old ones go only once its new one exists; stale holders go after the new
    // ones are made (never a window where an image another holder covered is unheld).
    for (image, name) in &p.renewed {
        if held.contains(&image.as_str()) {
            if let Err(e) = remove(rt, name) {
                errs.push(e);
            }
        }
    }
    for name in &p.retire {
        if let Err(e) = remove(rt, name) {
            errs.push(e);
        }
    }
    let failed: Vec<&str> = p
        .create
        .iter()
        .map(|(i, _)| i.as_str())
        .filter(|i| !held.contains(i))
        .collect();
    memo_all(&failed);
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

/// The holders on the runtime now (empty where holding is not supported).
pub(crate) fn list(rt: &Runtime) -> Result<Vec<Holder>, String> {
    if !supported(rt) {
        return Ok(Vec::new());
    }
    run(rt, &list_argv())
        .map(|o| parse_list(&o))
        .map_err(|e| format!("listing holders: {e}"))
}

/// Whether `holders` hold `(image, id)`.
pub(crate) fn holds(holders: &[Holder], image: &str, id: &str) -> bool {
    holders
        .iter()
        .any(|h| h.image == image && bare(&h.id) == bare(id))
}

// The fixtures are shell stubs: unix only, as the container floor's own tests.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const H: u64 = 60 * 60;
    const NOW: u64 = 1_000_000_000;

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
    fn a_holder_is_created_never_started_offline_and_escapes_the_seat_sweep() {
        let v = create_argv("wicked-seat-base:node22-rust", "sha256:56a12663c21bffff", 7);
        assert_eq!(v[0], "create");
        assert!(v[2].starts_with("wicked-keep-wicked-seat-base-node22-rust-"));
        assert!(v[2].ends_with("-56a12663c21b-7"), "{}", v[2]);
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
        // A directory is never executable: a started holder runs nothing of the image.
        assert!(v.windows(2).any(|p| p[0] == "--entrypoint" && p[1] == "/"));
        assert_eq!(v.last().unwrap(), "sha256:56a12663c21bffff");
        assert!(!v.iter().any(|a| a == "run" || a == "start"));
        let sweep = crate::container_floor::sweep_argv(Duration::from_secs(3600));
        assert!(sweep[0].contains(&"label=wicked.seat=1".to_string()));
    }

    #[test]
    fn references_the_slug_spells_alike_get_their_own_names() {
        assert_ne!(
            holder_name("seat:a_b", "sha256:1", 1),
            holder_name("seat:a-b", "sha256:1", 1)
        );
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
    fn a_missing_holder_is_made_and_a_fresh_one_left_alone() {
        let hs = [h("k1", "wicked-seat-claude:1", "sha256:a", NOW - H)];
        let p = plan(
            &hs,
            &[
                w("wicked-seat-claude:1", "sha256:a"),
                w("wicked-floor:9", "sha256:f"),
            ],
            NOW,
        );
        assert_eq!(p.create, vec![w("wicked-floor:9", "sha256:f")]);
        assert!(p.renewed.is_empty() && p.retire.is_empty());
    }

    #[test]
    fn a_wanted_holder_past_its_lease_is_renewed_new_first() {
        let hs = [h("k-old", "wicked-seat-claude:1", "sha256:a", NOW - 7 * H)];
        let p = plan(&hs, &[w("wicked-seat-claude:1", "sha256:a")], NOW);
        assert_eq!(p.create, vec![w("wicked-seat-claude:1", "sha256:a")]);
        assert_eq!(p.renewed, vec![w("wicked-seat-claude:1", "k-old")]);
        assert!(p.retire.is_empty());
    }

    #[test]
    fn another_daemons_tags_stay_held_while_their_lease_is_live() {
        // Daemon A runs tag 1 (renewed 5 h ago), an exited daemon left tag 2 (4 days), daemon B
        // holds tag 3 and the floor: only the unrenewed one goes, whatever the repository.
        let hs = [
            h("k-1", "wicked-seat-claude:1", "sha256:1", NOW - 5 * H),
            h("k-2", "wicked-seat-claude:2", "sha256:2", NOW - 96 * H),
            h("k-3", "wicked-seat-claude:3", "sha256:3", NOW - 10),
        ];
        let p = plan(
            &hs,
            &[
                w("wicked-seat-claude:3", "sha256:3"),
                w("wicked-floor:9", "sha256:f"),
            ],
            NOW,
        );
        assert_eq!(p.retire, vec!["k-2".to_string()]);
        assert_eq!(p.create, vec![w("wicked-floor:9", "sha256:f")]);
    }

    #[test]
    fn a_moved_tag_keeps_its_old_image_held_until_the_lease_runs_out() {
        // The tag moved (base rebuilt): the old image may still be what a slower engine just read.
        let hs = [h(
            "k-old",
            "wicked-seat-base:node22-rust",
            "sha256:old",
            NOW - H,
        )];
        let p = plan(&hs, &[w("wicked-seat-base:node22-rust", "sha256:new")], NOW);
        assert_eq!(
            p.create,
            vec![w("wicked-seat-base:node22-rust", "sha256:new")]
        );
        assert!(p.retire.is_empty());
        let later = plan(
            &hs,
            &[w("wicked-seat-base:node22-rust", "sha256:new")],
            NOW + 80 * H,
        );
        assert_eq!(later.retire, vec!["k-old".to_string()]);
    }

    fn stub(dir: &std::path::Path, body: &str) -> Runtime {
        // Named `docker`: holding runs only on the docker CLI.
        let rt = dir.join("docker");
        crate::test_env::install_stub(&rt, body);
        Runtime::answering(rt)
    }

    fn temp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wk-keep-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hold_makes_the_missing_holder_and_retires_a_stale_one_with_its_volumes() {
        let dir = temp();
        let log = dir.join("calls.log");
        let rt = stub(
            &dir,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n\
                 case \"$1 $2\" in\n\
                 'image inspect') case \"$*\" in *Volumes*) echo null ;; *) echo sha256:{} ;; esac ;;\n\
                 'ps -a') printf 'wicked-keep-stale\\tseat-x:0\\tsha256:old\\t1\\n' ;;\n\
                 esac\nexit 0\n",
                log.display(),
                "a".repeat(64)
            ),
        );
        hold_now(&rt, &["seat-x:1"]).unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("rm -v wicked-keep-stale"), "{calls}");
        assert!(
            calls.contains("create --name wicked-keep-seat-x-1-"),
            "{calls}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_image_that_declares_a_volume_is_not_held() {
        let dir = temp();
        let log = dir.join("calls.log");
        let rt = stub(
            &dir,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n\
                 case \"$1 $2\" in\n\
                 'image inspect') case \"$*\" in *Volumes*) echo '{{\"/data\":{{}}}}' ;; *) echo sha256:{} ;; esac ;;\n\
                 esac\nexit 0\n",
                log.display(),
                "c".repeat(64)
            ),
        );
        let e = hold_now(&rt, &["op/seat:1"]).unwrap_err();
        assert!(e.contains("declares a volume"), "{e}");
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(!calls.contains("create"), "{calls}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_name_taken_by_something_else_is_not_counted_as_held() {
        let dir = temp();
        let rt = stub(
            &dir,
            &format!(
                "#!/bin/sh\ncase \"$1 $2\" in\n\
                 'image inspect') case \"$*\" in *Volumes*) echo null ;; *) echo sha256:{} ;; esac ;;\n\
                 'ps -a') ;;\n\
                 'create --name') echo 'Conflict. The container name is already in use' >&2; exit 1 ;;\n\
                 'container inspect') printf '\\tother:1\\tsha256:zzz\\n' ;;\n\
                 esac\nexit 0\n",
                "d".repeat(64)
            ),
        );
        assert!(hold_now(&rt, &["seat-y:1"]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_tag_that_moved_after_it_was_read_is_not_held_by_this_engine() {
        // The first inspect says A, the second (just before the create) says B: another engine
        // rebuilt it; this one holds nothing and removes nothing.
        let dir = temp();
        let log = dir.join("calls.log");
        let n = dir.join("n");
        let rt = stub(
            &dir,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 case \"$1 $2\" in\n\
                 'image inspect') echo x >> '{n}'; if [ $(wc -l < '{n}') -le 1 ]; then echo sha256:{a}; else echo sha256:{b}; fi ;;\n\
                 'ps -a') ;;\n\
                 esac\nexit 0\n",
                log = log.display(),
                n = n.display(),
                a = "a".repeat(64),
                b = "b".repeat(64)
            ),
        );
        let e = hold_now(&rt, &["seat-z:1"]).unwrap_err();
        assert!(e.contains("changed while it was being held"), "{e}");
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            !calls.contains("create") && !calls.contains("rm "),
            "{calls}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_runtime_other_than_docker_is_not_held() {
        let dir = temp();
        let rt = dir.join("nerdctl");
        crate::test_env::install_stub(&rt, "#!/bin/sh\necho called >> \"$0.log\"\nexit 0\n");
        let rt = Runtime::answering(rt);
        assert!(!supported(&rt));
        hold_now(&rt, &["seat:1"]).unwrap();
        assert!(!dir.join("nerdctl.log").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
