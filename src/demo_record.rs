//! (DES-DEMO-RECORD-001, core#1249) The engine's half of a Demo video: the demo root, the two Tools that
//! run on the host (`demo_probe`, `demo_record`), the jail they run in, and how their result folds.
//!
//! A Demo video used to be recorded by a creator seat inside its container, which cannot reach the
//! person's app (its loopback is its own) and has no ffmpeg or browser. The recording now happens on
//! the host, in the engine's jail, from DATA the plan seat wrote and the person approved.
//!
//! ## Roots
//!
//! The launcher (crew's demo route) mints the DEMO ROOT and hands it as the run's
//! `LaunchSpec::evidence_root`; it writes `<root>/launch.json` (the app's loopback origin, the launch
//! path, the target crew decided, whether the app's rows may reach a model) before the launch. The
//! run's only write root is `<root>/plan`.
//!
//! | Path | Written by |
//! |---|---|
//! | `plan/` | crew (BRIEF.md) and the `plan` seat |
//! | `launch.json` | crew, before the launch |
//! | `probe/` | only the jailed `demo_probe` Tool |
//! | `approved/<gen>/`, `approved/current.json` | only the engine, when the `plan` unit is done |
//! | `demo-video/`, `review/` | only the jailed `demo_record` Tool |
//! | `takes/<n>/` | only the engine, rotating the previous take away before a new one |
//!
//! ## The jail
//!
//! Both Tools run under the validator's OS launcher with [`crate::validator::port_jail`]: writes only in
//! their own directories and a private temp dir, network to `localhost:<the app's port>` outbound
//! only. A host with no such jail (Linux: bwrap's `--unshare-net` hides the host's app; Windows) runs
//! NO recorder: the engine writes the verdict file as `INCONCLUSIVE` itself and says why.
//!
//! ## The fold
//!
//! The Tool writes its verdict file (`probe/result.json`, `demo-video/record.json`). On the worker
//! thread, after the Tool exits, the engine reads it and hashes every file the take wrote into a
//! [`DemoTake`] receipt carried to the fold in memory ([`crate::workflow::UnitEvidence::demo_take`]).
//! The fold passes a `demo_record` unit only on a PASS whose storyline digest is the approved one and
//! whose receipt holds every file the approved storyline implies. A refusal (the host could not
//! record) is denied with source [`DENIAL_REFUSED`] (the gate offers a retry); a take that ran and
//! is not PASS with [`DENIAL_VERDICT`] (send-back to the plan step).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The catalog id of the read-only page probe (neutral, engine-run Tool).
pub const PROBE_CATALOG_ID: &str = "demo_probe";
/// The catalog id of the recorder (neutral, engine-run Tool).
pub const RECORD_CATALOG_ID: &str = "demo_record";
pub(crate) const PROBE_CATALOG: &str = PROBE_CATALOG_ID;
pub(crate) const RECORD_CATALOG: &str = RECORD_CATALOG_ID;
/// The probe's fixed command: garden's demo recorder, action `probe`.
pub const DEMO_PROBE_CMD: [&str; 4] = [
    "wicked-garden",
    "run",
    "scripts/demo/demo-record.mjs",
    "probe",
];
/// The recorder's fixed command: garden's demo recorder, action `record`.
pub const DEMO_RECORD_CMD: [&str; 4] = [
    "wicked-garden",
    "run",
    "scripts/demo/demo-record.mjs",
    "record",
];

/// `UnitDenial.source` of a take the host could not make (deps, jail, app unreachable, capture): the
/// command result, so the gate re-runs the Tool ([`crate::actor`]'s rerun set).
pub(crate) const DENIAL_REFUSED: &str = "demo_refused";
/// `UnitDenial.source` of a take that ran and is not PASS: a verdict on the storyline, so send-back
/// goes to the plan step.
pub(crate) const DENIAL_VERDICT: &str = "demo_verdict";

/// The verdict causes that are refusals — the host could not record. Every other non-PASS cause is
/// a verdict on the take. A superset of garden's `demo-record.mjs` `REFUSAL_CAUSES`: theirs plus the
/// ones only the engine writes (`no_port_scoped_jail`, `deadline_exceeded`, `budget_exceeded`).
pub(crate) const REFUSAL_CAUSES: &[&str] = &[
    "unjailed_host",
    "recorder_deps_missing",
    "engine_refused",
    "app_unreachable",
    "encoder_failed",
    "capture_stalled",
    "no_port_scoped_jail",
    "deadline_exceeded",
    "budget_exceeded",
];

/// The launch manifest crew writes, in the demo root.
pub(crate) const LAUNCH_FILE: &str = "launch.json";
/// The seat's write root under the demo root.
pub(crate) const PLAN_DIR: &str = "plan";
pub(crate) const PROBE_DIR: &str = "probe";
pub(crate) const APPROVED_DIR: &str = "approved";
pub(crate) const VIDEO_DIR: &str = "demo-video";
pub(crate) const REVIEW_DIR: &str = "review";
pub(crate) const TAKES_DIR: &str = "takes";
/// The approved generation pointer under `approved/`.
pub(crate) const CURRENT_FILE: &str = "current.json";
/// The plan files the engine snapshots when the plan unit is done.
pub(crate) const PLAN_FILES: [&str; 2] = ["storyline.json", "chapters.json"];
/// The longest a take may run before the engine kills it.
pub(crate) const TAKE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20 * 60);
/// The most a take may write (its output dirs and its private temp dir), polled while it runs.
pub(crate) const TAKE_BYTE_BUDGET: u64 = 2 * 1024 * 1024 * 1024;
/// The largest plan file the engine copies into the approved snapshot.
const PLAN_FILE_MAX: u64 = 64 * 1024;
/// How many archived takes the engine keeps.
const TAKES_KEPT: usize = 2;

/// What the launcher decided about the app (`<root>/launch.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DemoLaunch {
    /// The app's loopback origin: `http://127.0.0.1:<port>`, `http://localhost:<port>` or `http://[::1]:<port>`.
    pub(crate) app_origin: String,
    /// The path (and query) the person launched; every navigation stays under its pathname.
    pub(crate) launch_path: String,
    /// `{"kind":"crew_app","id":…}` or `{"kind":"loopback_app"}`, decided by crew from its own state.
    pub(crate) target: serde_json::Value,
    /// `false` when the app's rows may not reach a model (DES-APPS-001 S5).
    pub(crate) live_data_to_models: bool,
}

impl DemoLaunch {
    /// The app's TCP port (the one port the jail opens).
    pub(crate) fn port(&self) -> Result<u16, String> {
        let rest = self
            .app_origin
            .strip_prefix("http://")
            .or_else(|| self.app_origin.strip_prefix("https://"))
            .ok_or_else(|| format!("the app origin {:?} is not http(s)", self.app_origin))?;
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or_else(|| format!("the app origin {:?} names no port", self.app_origin))?;
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
            return Err(format!(
                "the app origin {:?} is not on this machine (127.0.0.1, localhost or [::1])",
                self.app_origin
            ));
        }
        port.parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| format!("the app origin {:?} has no valid port", self.app_origin))
    }

    fn check(&self) -> Result<(), String> {
        self.port()?;
        let lp = &self.launch_path;
        let plain = lp.starts_with('/')
            && !lp.starts_with("//")
            && lp.len() <= 2048
            && lp
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._~%/?&=+-".contains(c));
        if !plain {
            return Err(format!("the launch path {lp:?} is not a plain path"));
        }
        match self.target.get("kind").and_then(|k| k.as_str()) {
            Some("loopback_app") => Ok(()),
            Some("crew_app") => {
                let id = self.target.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let ok = !id.is_empty()
                    && id.len() <= 128
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
                ok.then_some(())
                    .ok_or_else(|| format!("the crew app id {id:?} is not a plain name"))
            }
            other => Err(format!("unknown demo target kind {other:?}")),
        }
    }
}

/// Read and check `<root>/launch.json`.
pub(crate) fn read_launch(root: &Path) -> Result<DemoLaunch, String> {
    let file = root.join(LAUNCH_FILE);
    let meta = std::fs::symlink_metadata(&file)
        .map_err(|e| format!("the demo root has no {LAUNCH_FILE}: {e}"))?;
    if !meta.is_file() || meta.len() > PLAN_FILE_MAX {
        return Err(format!("{} is not a small regular file", file.display()));
    }
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let launch: DemoLaunch = serde_json::from_str(&text)
        .map_err(|e| format!("{LAUNCH_FILE} is not the launch manifest: {e}"))?;
    launch.check()?;
    Ok(launch)
}

/// Whether a plan or preset names a demo Tool (only such a run needs a demo root).
pub(crate) fn is_demo_catalog(catalog: Option<&str>) -> bool {
    matches!(catalog, Some(PROBE_CATALOG) | Some(RECORD_CATALOG))
}

/// The built-in Demo video presets: with the model review step, and without it (an app whose rows may
/// not reach a model, DES-DEMO-RECORD-001 §3.7).
pub const DEMO_PRESETS: [&str; 2] = ["demo", "demo-human-review"];

/// Whether a launch runs a demo Tool: a demo preset, or a plan naming `demo_probe`/`demo_record`.
pub(crate) fn launch_is_demo(
    workflow: Option<&str>,
    plan: Option<&crate::plan::PlanSteps>,
) -> bool {
    workflow.is_some_and(|w| DEMO_PRESETS.contains(&w))
        || plan.is_some_and(|p| {
            p.steps
                .iter()
                .any(|s| is_demo_catalog(Some(s.catalog.as_str())))
        })
}

/// (Launch) A run that carries a demo Tool needs a demo root with a valid manifest, and no declared
/// write root may be the demo root or hold it: a seat must never write where the Tools write.
pub(crate) fn validate_demo_launch(
    evidence_root: Option<&str>,
    extra_write_roots: &[String],
) -> Result<(), String> {
    let root = evidence_root.ok_or(
        "a Demo video run needs its demo root (the launcher's evidence root); none was handed",
    )?;
    let root = Path::new(root);
    read_launch(root)?;
    // The worker boundary's own resolution (nearest existing ancestor), so a write root spelled through a
    // link cannot look outside the demo root here and land inside it there.
    let real = |p: &Path| {
        crate::worker_sandbox::canon_lenient(p)
            .ok_or_else(|| format!("{} cannot be resolved", p.display()))
    };
    let root_real = real(root)?;
    let plan_real = root_real.join(PLAN_DIR);
    for w in extra_write_roots {
        let w_real = real(Path::new(w))?;
        if root_real.starts_with(&w_real) {
            return Err(format!(
                "the write root {w} holds the demo root: a seat could write the recording itself; \
                 declare only {}",
                root.join(PLAN_DIR).display()
            ));
        }
        if w_real.starts_with(&root_real) && !w_real.starts_with(&plan_real) {
            return Err(format!(
                "the write root {w} is inside the demo root but outside its plan directory"
            ));
        }
    }
    Ok(())
}

/// (Dispatch) The same admission again, on what the run actually holds (a saved preset, a plan
/// revision or a launch path that never named a demo Tool meets it here before a Tool runs), plus
/// the run's own working directory (its worktree or run sandbox), which every seat can write.
pub(crate) fn validate_demo_session(
    session: &crate::domain::AgentSession,
    run_id: &str,
) -> Result<(), String> {
    validate_demo_launch(session.evidence_root.as_deref(), &session.extra_write_roots)?;
    let root = Path::new(session.evidence_root.as_deref().unwrap_or_default());
    let root_real = crate::worker_sandbox::canon_lenient(root)
        .ok_or_else(|| format!("{} cannot be resolved", root.display()))?;
    let workdir = session
        .workdir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::execute_wrapped::sandbox_root(run_id));
    if let Some(w) = crate::worker_sandbox::canon_lenient(&workdir) {
        if root_real.starts_with(&w) || w.starts_with(&root_real) {
            return Err(format!(
                "the run's working directory {} overlaps the demo root: a seat could write the \
                 recording",
                workdir.display()
            ));
        }
    }
    Ok(())
}

/// A directory the engine owns under the demo root, checked at use time: a plain directory (not a
/// link) directly under the root. Created when missing.
pub(crate) fn owned_dir(root: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = root.join(name);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            return Err(format!("{} is not a plain directory", dir.display()))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?
        }
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    crate::walkthrough::check_proof_root(&dir)?;
    Ok(dir)
}

/// The approved snapshot's pointer (`approved/current.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Approved {
    pub(crate) gen: u32,
    /// The run whose plan step this snapshot is (a snapshot is never another run's approval).
    #[serde(default)]
    pub(crate) run: String,
    /// sha256 of `storyline.json` as approved.
    pub(crate) storyline_sha: String,
    /// The chapter keys of `chapters.json`, in order (the files a take must produce follow from them).
    pub(crate) chapter_keys: Vec<String>,
    /// Whether the storyline has an intro (a segment the take records too).
    pub(crate) intro: bool,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Read a plan file the seat wrote: a small regular file, never through a link.
fn read_plan_file(dir: &Path, name: &str) -> Result<Vec<u8>, String> {
    let f = dir.join(name);
    let m = std::fs::symlink_metadata(&f).map_err(|e| format!("the plan has no {name}: {e}"))?;
    if !m.is_file() {
        return Err(format!("the plan's {name} is not a regular file"));
    }
    if m.len() > PLAN_FILE_MAX {
        return Err(format!(
            "the plan's {name} is larger than {PLAN_FILE_MAX} bytes"
        ));
    }
    std::fs::read(&f).map_err(|e| format!("the plan's {name}: {e}"))
}

/// (When the `plan` unit is done) Copy the plan's storyline and chapters into a new
/// `approved/<gen>/` and point `approved/current.json` at it. The person's gate shows this snapshot
/// and the recorder reads only it, so what records is what was approved.
pub(crate) fn snapshot_plan(root: &Path, run_id: &str) -> Result<Approved, String> {
    let plan = root.join(PLAN_DIR);
    let story = read_plan_file(&plan, PLAN_FILES[0])?;
    let chapters = read_plan_file(&plan, PLAN_FILES[1])?;
    let parsed: serde_json::Value = serde_json::from_slice(&chapters)
        .map_err(|e| format!("the plan's chapters.json is not JSON: {e}"))?;
    let chapter_keys: Vec<String> = parsed
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.get("key").and_then(|k| k.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let intro = serde_json::from_slice::<serde_json::Value>(&story)
        .ok()
        .and_then(|v| v.get("intro").cloned())
        .is_some_and(|i| !i.is_null());
    let approved = owned_dir(root, APPROVED_DIR)?;
    let gen = std::fs::read_dir(&approved)
        .map_err(|e| format!("{}: {e}", approved.display()))?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .max()
        .map_or(1, |g| g + 1);
    let dir = approved.join(gen.to_string());
    std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(dir.join(PLAN_FILES[0]), &story).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(PLAN_FILES[1]), &chapters).map_err(|e| e.to_string())?;
    let a = Approved {
        gen,
        run: run_id.to_string(),
        storyline_sha: sha256_hex(&story),
        chapter_keys,
        intro,
    };
    let tmp = approved.join(format!(".{CURRENT_FILE}.{}", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&a).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, approved.join(CURRENT_FILE)).map_err(|e| e.to_string())?;
    Ok(a)
}

/// The current approved snapshot of `run_id`, when there is one. Every path is the engine's own and
/// checked: no linked `approved/`, no linked `current.json`, no linked generation.
pub(crate) fn current_approved(root: &Path, run_id: &str) -> Result<(Approved, PathBuf), String> {
    let approved = root.join(APPROVED_DIR);
    crate::walkthrough::check_proof_root(&approved)?;
    let cur = approved.join(CURRENT_FILE);
    let m =
        std::fs::symlink_metadata(&cur).map_err(|e| format!("no approved plan snapshot: {e}"))?;
    if !m.is_file() || m.len() > PLAN_FILE_MAX {
        return Err("approved/current.json is not a small regular file".to_string());
    }
    let text = std::fs::read_to_string(&cur).map_err(|e| format!("approved/current.json: {e}"))?;
    let a: Approved =
        serde_json::from_str(&text).map_err(|e| format!("approved/current.json: {e}"))?;
    if a.run != run_id {
        return Err(format!(
            "the approved snapshot is run {:?}'s, not this run's: a run records only the plan it \
             approved",
            a.run
        ));
    }
    let dir = approved.join(a.gen.to_string());
    crate::walkthrough::check_proof_root(&dir)?;
    Ok((a, dir))
}

/// Remove a directory tree without following links (the engine's own directories only).
fn remove_tree(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// (Before a take) Move the previous take's `demo-video/` and `review/` to `takes/<n>/`, recreate both
/// empty, and keep only the last [`TAKES_KEPT`] archived takes.
pub(crate) fn rotate_take(root: &Path) -> Result<(), String> {
    let takes = owned_dir(root, TAKES_DIR)?;
    let mut have: Vec<u32> = std::fs::read_dir(&takes)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .collect();
    have.sort_unstable();
    let next = have.last().map_or(1, |n| n + 1);
    let dest = takes.join(next.to_string());
    let mut moved = false;
    for name in [VIDEO_DIR, REVIEW_DIR] {
        let src = root.join(name);
        match std::fs::symlink_metadata(&src) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
                if std::fs::read_dir(&src)
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(false)
                {
                    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
                    std::fs::rename(&src, dest.join(name)).map_err(|e| e.to_string())?;
                    moved = true;
                }
            }
            Ok(_) => remove_tree(&src).map_err(|e| format!("{}: {e}", src.display()))?,
            Err(_) => {}
        }
        owned_dir(root, name)?;
    }
    if moved {
        have.push(next);
    }
    while have.len() > TAKES_KEPT {
        let old = have.remove(0);
        remove_tree(&takes.join(old.to_string())).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// What a demo Tool's run left, read on the worker thread right after it exited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemoTake {
    /// `PASS`, `FAIL` or `INCONCLUSIVE` (`""` when the verdict file was missing or unreadable).
    pub overall: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storyline_sha: Option<String>,
    /// Every regular file the take wrote under its directories, root-relative, with its sha256.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    /// Why the receipt itself could not be taken (a link, an unreadable file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_error: Option<String>,
    /// The chapters that failed: `key · cause at m:ss`, for the send-back note. Only these three fields
    /// are kept (never a message, which could carry page text).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_chapters: Vec<String>,
}

impl DemoTake {
    /// A take the engine refused to start (no jail, no launch manifest, …).
    pub(crate) fn refused(cause: &str, reason: String) -> Self {
        Self {
            overall: "INCONCLUSIVE".to_string(),
            cause: Some(cause.to_string()),
            reason: Some(reason),
            ..Self::default()
        }
    }
}

/// Hash every regular file under `dirs` (root-relative names), refusing links and non-regular entries.
fn receipt(root: &Path, dirs: &[&str]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<(), String> {
        for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let e = e.map_err(|e| e.to_string())?;
            let p = e.path();
            let m = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
            let rel = p
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if m.file_type().is_symlink() {
                return Err(format!("{rel} is a link"));
            } else if m.is_dir() {
                walk(root, &p, out)?;
            } else if m.is_file() {
                use sha2::Digest;
                let mut h = sha2::Sha256::new();
                let mut f = std::fs::File::open(&p).map_err(|e| format!("{rel}: {e}"))?;
                std::io::copy(&mut f, &mut h).map_err(|e| format!("{rel}: {e}"))?;
                out.insert(rel, format!("{:x}", h.finalize()));
            } else {
                return Err(format!("{rel} is not a regular file"));
            }
        }
        Ok(())
    }
    for d in dirs {
        let dir = root.join(d);
        if dir.exists() {
            walk(root, &dir, &mut out)?;
        }
    }
    Ok(out)
}

/// The failed chapters of a verdict file: `key · cause at m:ss` (at most 12; keys and causes bounded).
fn failed_chapters(v: &serde_json::Value) -> Vec<String> {
    v["chapters"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .filter(|c| c["status"].as_str() == Some("failed"))
                .take(12)
                .map(|c| {
                    let key: String = c["key"].as_str().unwrap_or("?").chars().take(48).collect();
                    let code: String = c["code"]
                        .as_str()
                        .unwrap_or("step_failed")
                        .chars()
                        .take(40)
                        .collect();
                    let at = c["failed_at_sec"].as_f64().map_or(String::new(), |s| {
                        let s = s.max(0.0) as u64;
                        format!(" at {}:{:02}", s / 60, s % 60)
                    });
                    format!("{key} · {code}{at}")
                })
                .collect()
        })
        .unwrap_or_default()
}

/// (Worker thread, after the Tool exited) Read the verdict file and take the receipt.
pub(crate) fn read_take(root: &Path, catalog: &str) -> DemoTake {
    let (verdict, dirs): (PathBuf, &[&str]) = if catalog == PROBE_CATALOG {
        (root.join(PROBE_DIR).join("result.json"), &[PROBE_DIR])
    } else {
        (
            root.join(VIDEO_DIR).join("record.json"),
            &[VIDEO_DIR, REVIEW_DIR],
        )
    };
    let mut take: DemoTake = std::fs::read_to_string(&verdict)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|v| DemoTake {
            overall: v["overall"].as_str().unwrap_or_default().to_string(),
            cause: v["cause"].as_str().map(str::to_string),
            reason: v["reason"].as_str().map(|r| r.chars().take(600).collect()),
            storyline_sha: v["storyline_sha"].as_str().map(str::to_string),
            failed_chapters: failed_chapters(&v),
            ..DemoTake::default()
        })
        .unwrap_or_default();
    match receipt(root, dirs) {
        Ok(files) => take.files = files,
        Err(e) => take.receipt_error = Some(e),
    }
    take
}

/// The files a `demo_record` take must have written, from the approved plan.
pub(crate) fn expected_files(a: &Approved) -> Vec<String> {
    let mut v: Vec<String> = [
        "demo-video/demo.mp4",
        "demo-video/chapters.md",
        "demo-video/timings.json",
        "demo-video/record.json",
        "review/chapters.png",
        "review/joins.png",
        "review/end.png",
        "review/ffprobe.json",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if a.intro {
        v.push("demo-video/segments/intro/segment.mp4".to_string());
    }
    for k in &a.chapter_keys {
        v.push(format!("demo-video/segments/{k}/segment.mp4"));
    }
    v
}

/// The fold's reading of a demo Tool's take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Judgement {
    Pass,
    /// The host could not record: [`DENIAL_REFUSED`].
    Refused(String),
    /// The take ran and is not PASS: [`DENIAL_VERDICT`].
    Verdict(String),
}

/// Judge a take. `approved` is the current approved snapshot (`None` for the probe, which has no
/// storyline, or when there is none — a `demo_record` take with none is refused).
pub(crate) fn judge(
    catalog: &str,
    take: Option<&DemoTake>,
    approved: Option<&Approved>,
) -> Judgement {
    let Some(take) = take else {
        return Judgement::Refused("the engine took no receipt of this take".to_string());
    };
    let why = || {
        format!(
            "{}{}{}",
            take.cause.as_deref().unwrap_or("no verdict"),
            take.reason
                .as_deref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default(),
            if take.failed_chapters.is_empty() {
                String::new()
            } else {
                format!(" (failed: {})", take.failed_chapters.join("; "))
            }
        )
    };
    if let Some(e) = &take.receipt_error {
        return Judgement::Refused(format!("the take's files could not be receipted: {e}"));
    }
    if take.overall != "PASS" {
        // The probe has no storyline to send back to (it runs before the plan): any miss is a retry.
        let refusal = catalog == PROBE_CATALOG
            || take.overall.is_empty()
            || take
                .cause
                .as_deref()
                .is_none_or(|c| REFUSAL_CAUSES.contains(&c));
        return if refusal {
            Judgement::Refused(if take.overall.is_empty() {
                "the Tool wrote no verdict file".to_string()
            } else {
                why()
            })
        } else {
            Judgement::Verdict(why())
        };
    }
    if catalog == PROBE_CATALOG {
        return Judgement::Pass;
    }
    let Some(a) = approved else {
        return Judgement::Refused(
            "no approved plan snapshot to judge the take against".to_string(),
        );
    };
    if take.storyline_sha.as_deref() != Some(a.storyline_sha.as_str()) {
        return Judgement::Refused(
            "the take did not record the approved storyline (its digest differs)".to_string(),
        );
    }
    let missing: Vec<String> = expected_files(a)
        .into_iter()
        .filter(|f| !take.files.contains_key(f))
        .collect();
    if !missing.is_empty() {
        return Judgement::Verdict(format!(
            "the take reported PASS but did not write {}",
            missing.join(", ")
        ));
    }
    Judgement::Pass
}

/// The inconclusive verdict file the engine writes itself when it cannot run the Tool.
pub(crate) fn write_refusal(root: &Path, catalog: &str, cause: &str, reason: &str) {
    let (dir, file) = if catalog == PROBE_CATALOG {
        (PROBE_DIR, "result.json")
    } else {
        (VIDEO_DIR, "record.json")
    };
    if let Ok(d) = owned_dir(root, dir) {
        // Never through a link planted at the verdict's name: a fresh file, renamed over it.
        let tmp = d.join(format!(".{file}.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let body = serde_json::json!({"overall": "INCONCLUSIVE", "cause": cause, "reason": reason})
            .to_string();
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .and_then(|mut f| std::io::Write::write_all(&mut f, body.as_bytes()));
        if written.is_ok() {
            let _ = std::fs::rename(&tmp, d.join(file));
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Bytes under `dirs` (no links followed), for the take's byte budget.
pub(crate) fn bytes_under(dirs: &[&Path]) -> u64 {
    fn walk(p: &Path) -> u64 {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return 0;
        };
        if m.is_dir() && !m.file_type().is_symlink() {
            std::fs::read_dir(p)
                .map(|rd| rd.filter_map(Result::ok).map(|e| walk(&e.path())).sum())
                .unwrap_or(0)
        } else {
            m.len()
        }
    }
    dirs.iter().map(|d| walk(d)).sum()
}

/// The variables a demo Tool is handed on top of the private-temp ones (see the module docs).
pub(crate) fn tool_env(
    run_id: &str,
    ord: u32,
    root: &Path,
    launch: &DemoLaunch,
    storyline: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("WICKED_RUN_ID".to_string(), run_id.to_string()),
        ("WICKED_RUN_UNIT".to_string(), ord.to_string()),
        (
            "WICKED_DEMO_ROOT".to_string(),
            root.to_string_lossy().into_owned(),
        ),
        (
            "WICKED_DEMO_APP_ORIGIN".to_string(),
            launch.app_origin.clone(),
        ),
        (
            "WICKED_DEMO_LAUNCH_PATH".to_string(),
            launch.launch_path.clone(),
        ),
        ("WICKED_DEMO_TARGET".to_string(), launch.target.to_string()),
        (
            "WICKED_DEMO_LIVE_DATA_TO_MODELS".to_string(),
            if launch.live_data_to_models { "1" } else { "0" }.to_string(),
        ),
    ];
    if let Some(s) = storyline {
        env.push((
            "WICKED_DEMO_STORYLINE".to_string(),
            s.to_string_lossy().into_owned(),
        ));
    }
    // A crew app's palette is crew's own (the same port the jail opens).
    if launch.target.get("kind").and_then(|k| k.as_str()) == Some("crew_app") {
        env.push(("WICKED_CREW_URL".to_string(), launch.app_origin.clone()));
    }
    env
}

/// The host tools the recorder needs, resolved to absolute paths outside the jail: `FFMPEG`, `PYTHON`,
/// `WICKED_DEMO_DEPS`, `PLAYWRIGHT_BROWSERS_PATH`, `HOME`. A missing one is left unset (the recorder
/// names it as missing; nothing is installed).
pub(crate) fn host_tool_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    let path = std::env::var_os("PATH");
    let find = |name: &str| -> Option<PathBuf> {
        let dirs = std::env::split_paths(path.as_ref()?).collect::<Vec<_>>();
        dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
    };
    let ffmpeg = std::env::var_os("WICKED_FFMPEG")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| find("ffmpeg"));
    if let Some(f) = ffmpeg {
        env.push(("FFMPEG".to_string(), f.to_string_lossy().into_owned()));
    }
    if let Some(p) = find("python3") {
        env.push(("PYTHON".to_string(), p.to_string_lossy().into_owned()));
    }
    for k in [
        "WICKED_DEMO_DEPS",
        "PLAYWRIGHT_BROWSERS_PATH",
        "WICKED_GARDEN_CACHE_DIR",
        "XDG_CACHE_HOME",
        "HOME",
    ] {
        if let Some(v) = std::env::var_os(k) {
            env.push((k.to_string(), v.to_string_lossy().into_owned()));
        }
    }
    env
}

/// Everything the actor resolves (on its own thread, with the session) for one demo Tool dispatch.
#[derive(Debug, Clone)]
pub(crate) struct DemoDispatch {
    pub(crate) catalog: String,
    pub(crate) run_id: String,
    pub(crate) ord: u32,
    /// The demo root, or why there is none.
    pub(crate) root: Result<PathBuf, String>,
    pub(crate) launch: Result<DemoLaunch, String>,
    /// (`demo_record`) the approved snapshot dir, or why there is none.
    pub(crate) approved: Option<Result<PathBuf, String>>,
}

impl DemoDispatch {
    pub(crate) fn resolve(
        run_id: &str,
        session: &crate::domain::AgentSession,
        unit: &crate::domain::WorkUnit,
    ) -> Self {
        let catalog = unit.catalog.clone().unwrap_or_default();
        let root = session
            .evidence_root
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| "the launcher minted no demo root for this run".to_string())
            .and_then(|r| crate::walkthrough::check_proof_root(&r).map(|()| r));
        let root = root.and_then(|r| validate_demo_session(session, run_id).map(|()| r));
        let launch = root.clone().and_then(|r| read_launch(&r));
        let approved = (catalog == RECORD_CATALOG).then(|| {
            root.clone()
                .and_then(|r| current_approved(&r, run_id).map(|(_, d)| d))
        });
        Self {
            catalog,
            run_id: run_id.to_string(),
            ord: unit.ord,
            root,
            launch,
            approved,
        }
    }

    /// The directories this Tool may write.
    pub(crate) fn out_dirs(&self) -> &'static [&'static str] {
        if self.catalog == PROBE_CATALOG {
            &[PROBE_DIR]
        } else {
            &[VIDEO_DIR, REVIEW_DIR]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wicked-demo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn launch_json(root: &Path, origin: &str) {
        std::fs::write(
            root.join(LAUNCH_FILE),
            serde_json::json!({"app_origin": origin, "launch_path": "/apps/esc",
                "target": {"kind": "crew_app", "id": "esc"}, "live_data_to_models": true})
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_demo_launch_needs_a_loopback_app_and_a_plan_only_write_root() {
        let root = tmp("launch");
        launch_json(&root, "http://127.0.0.1:7721");
        let r = root.to_string_lossy().into_owned();
        let plan = root.join(PLAN_DIR).to_string_lossy().into_owned();
        assert!(validate_demo_launch(Some(&r), &[plan]).is_ok());
        // The whole root as a write root: a seat could write the recording.
        let e = validate_demo_launch(Some(&r), std::slice::from_ref(&r)).unwrap_err();
        assert!(e.contains("holds the demo root"), "{e}");
        let e = validate_demo_launch(
            Some(&r),
            &[root.join("demo-video").to_string_lossy().into_owned()],
        )
        .unwrap_err();
        assert!(e.contains("outside its plan directory"), "{e}");
        assert!(validate_demo_launch(None, &[]).is_err());
        launch_json(&root, "https://example.com:443");
        assert!(validate_demo_launch(Some(&r), &[])
            .unwrap_err()
            .contains("not on this machine"));
        launch_json(&root, "http://127.0.0.1");
        assert!(validate_demo_launch(Some(&r), &[])
            .unwrap_err()
            .contains("no port"));
        let _ = std::fs::remove_dir_all(root);
    }

    fn approved(keys: &[&str]) -> Approved {
        Approved {
            gen: 1,
            run: "r".into(),
            storyline_sha: "abc".into(),
            chapter_keys: keys.iter().map(|s| s.to_string()).collect(),
            intro: false,
        }
    }

    fn passing(a: &Approved) -> DemoTake {
        DemoTake {
            overall: "PASS".into(),
            storyline_sha: Some(a.storyline_sha.clone()),
            files: expected_files(a)
                .into_iter()
                .map(|f| (f, "h".to_string()))
                .collect(),
            ..DemoTake::default()
        }
    }

    #[test]
    fn a_take_passes_only_with_the_approved_storyline_and_every_file() {
        let a = approved(&["01-a", "02-b"]);
        assert_eq!(
            judge(RECORD_CATALOG, Some(&passing(&a)), Some(&a)),
            Judgement::Pass
        );
        // No receipt: never a pass.
        assert!(matches!(
            judge(RECORD_CATALOG, None, Some(&a)),
            Judgement::Refused(_)
        ));
        // Another storyline than the approved one.
        let mut t = passing(&a);
        t.storyline_sha = Some("other".into());
        assert!(matches!(
            judge(RECORD_CATALOG, Some(&t), Some(&a)),
            Judgement::Refused(_)
        ));
        // A chapter's segment missing.
        let mut t = passing(&a);
        t.files.remove("demo-video/segments/02-b/segment.mp4");
        assert!(
            matches!(judge(RECORD_CATALOG, Some(&t), Some(&a)), Judgement::Verdict(r) if r.contains("02-b"))
        );
        // No approved snapshot.
        assert!(matches!(
            judge(RECORD_CATALOG, Some(&passing(&a)), None),
            Judgement::Refused(_)
        ));
        // A receipt that met a link.
        let mut t = passing(&a);
        t.receipt_error = Some("x is a link".into());
        assert!(matches!(
            judge(RECORD_CATALOG, Some(&t), Some(&a)),
            Judgement::Refused(_)
        ));
    }

    #[test]
    fn a_refusal_retries_and_a_verdict_sends_back() {
        let a = approved(&["01-a"]);
        for (cause, refused) in [
            ("recorder_deps_missing", true),
            ("app_unreachable", true),
            ("capture_stalled", true),
            ("deadline_exceeded", true),
            ("left_app", false),
            ("side_effect_blocked", false),
            ("storyline_refused", false),
            ("chapter_mismatch", false),
        ] {
            let t = DemoTake {
                overall: if refused { "INCONCLUSIVE" } else { "FAIL" }.into(),
                cause: Some(cause.into()),
                ..DemoTake::default()
            };
            let j = judge(RECORD_CATALOG, Some(&t), Some(&a));
            assert_eq!(
                matches!(j, Judgement::Refused(_)),
                refused,
                "{cause}: {j:?}"
            );
        }
        // No verdict file at all: the Tool failed to run; retry.
        assert!(matches!(
            judge(RECORD_CATALOG, Some(&DemoTake::default()), Some(&a)),
            Judgement::Refused(_)
        ));
    }

    #[test]
    fn the_snapshot_is_what_the_plan_wrote_then_and_rotation_keeps_two_takes() {
        let root = tmp("snap");
        std::fs::create_dir_all(root.join(PLAN_DIR)).unwrap();
        std::fs::write(
            root.join("plan/storyline.json"),
            br#"{"intro":{"title":"x","ms":1500}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("plan/chapters.json"),
            br#"[{"key":"01-a"},{"key":"02-b"}]"#,
        )
        .unwrap();
        let a = snapshot_plan(&root, "run-a").unwrap();
        assert_eq!((a.gen, a.intro), (1, true));
        assert_eq!(a.chapter_keys, ["01-a", "02-b"]);
        // The seat edits the plan afterwards: the snapshot does not move.
        std::fs::write(root.join("plan/storyline.json"), b"{}").unwrap();
        let (cur, dir) = current_approved(&root, "run-a").unwrap();
        // Another run never records this run's approval.
        assert!(current_approved(&root, "run-b")
            .unwrap_err()
            .contains("not this run's"));
        assert_eq!(cur, a);
        assert_eq!(
            std::fs::read(dir.join("storyline.json")).unwrap(),
            br#"{"intro":{"title":"x","ms":1500}}"#
        );
        assert_eq!(snapshot_plan(&root, "run-a").unwrap().gen, 2);
        // A linked plan file is refused.
        #[cfg(unix)]
        {
            std::fs::remove_file(root.join("plan/chapters.json")).unwrap();
            std::os::unix::fs::symlink("/etc/hosts", root.join("plan/chapters.json")).unwrap();
            assert!(snapshot_plan(&root, "run-a")
                .unwrap_err()
                .contains("not a regular file"));
        }
        for i in 0..4 {
            owned_dir(&root, VIDEO_DIR).unwrap();
            std::fs::write(root.join(VIDEO_DIR).join("demo.mp4"), format!("take {i}")).unwrap();
            rotate_take(&root).unwrap();
            assert!(
                std::fs::read_dir(root.join(VIDEO_DIR))
                    .unwrap()
                    .next()
                    .is_none(),
                "fresh"
            );
        }
        let kept: Vec<_> = std::fs::read_dir(root.join(TAKES_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(kept.len(), TAKES_KEPT);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_receipt_refuses_a_link() {
        let root = tmp("receipt");
        owned_dir(&root, VIDEO_DIR).unwrap();
        owned_dir(&root, REVIEW_DIR).unwrap();
        std::fs::write(
            root.join("demo-video/record.json"),
            br#"{"overall":"PASS","storyline_sha":"s"}"#,
        )
        .unwrap();
        std::fs::write(root.join("review/end.png"), b"png").unwrap();
        let t = read_take(&root, RECORD_CATALOG);
        assert_eq!(t.overall, "PASS");
        assert!(
            t.files.contains_key("review/end.png")
                && t.files.contains_key("demo-video/record.json")
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hosts", root.join("review/x.png")).unwrap();
            assert!(read_take(&root, RECORD_CATALOG).receipt_error.is_some());
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_probe_miss_is_always_a_retry_and_a_failed_chapter_names_its_moment() {
        let t = DemoTake {
            overall: "FAIL".into(),
            cause: Some("left_app".into()),
            ..DemoTake::default()
        };
        assert!(matches!(
            judge(PROBE_CATALOG, Some(&t), None),
            Judgement::Refused(_)
        ));
        let root = tmp("failed-ch");
        owned_dir(&root, VIDEO_DIR).unwrap();
        std::fs::write(
            root.join("demo-video/record.json"),
            r#"{"overall":"FAIL","cause":"left_app","chapters":[{"key":"01-a","status":"ok"},{"key":"02-b","status":"failed","code":"left_app","failed_at_sec":75.4,"message":"SECRET"}]}"#,
        )
        .unwrap();
        let take = read_take(&root, RECORD_CATALOG);
        assert_eq!(take.failed_chapters, ["02-b · left_app at 1:15"]);
        let j = judge(
            RECORD_CATALOG,
            Some(&take),
            Some(&approved(&["01-a", "02-b"])),
        );
        assert!(
            matches!(&j, Judgement::Verdict(r) if r.contains("02-b · left_app at 1:15") && !r.contains("SECRET")),
            "{j:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_tool_env_is_exactly_the_declared_set() {
        let launch = DemoLaunch {
            app_origin: "http://127.0.0.1:7721".into(),
            launch_path: "/apps/esc".into(),
            target: serde_json::json!({"kind": "crew_app", "id": "esc"}),
            live_data_to_models: false,
        };
        let env = tool_env(
            "r",
            3,
            Path::new("/demo"),
            &launch,
            Some(Path::new("/demo/approved/1")),
        );
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "WICKED_RUN_ID",
                "WICKED_RUN_UNIT",
                "WICKED_DEMO_ROOT",
                "WICKED_DEMO_APP_ORIGIN",
                "WICKED_DEMO_LAUNCH_PATH",
                "WICKED_DEMO_TARGET",
                "WICKED_DEMO_LIVE_DATA_TO_MODELS",
                "WICKED_DEMO_STORYLINE",
                "WICKED_CREW_URL",
            ]
        );
        let allowed = [
            "FFMPEG",
            "PYTHON",
            "WICKED_DEMO_DEPS",
            "PLAYWRIGHT_BROWSERS_PATH",
            "WICKED_GARDEN_CACHE_DIR",
            "XDG_CACHE_HOME",
            "HOME",
        ];
        for (k, _) in host_tool_env() {
            assert!(allowed.contains(&k.as_str()), "{k}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_verdict_link_is_replaced_not_followed() {
        let root = tmp("refusal-link");
        let outside = root.join("outside.txt");
        std::fs::write(&outside, b"keep").unwrap();
        owned_dir(&root, VIDEO_DIR).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("demo-video/record.json")).unwrap();
        write_refusal(&root, RECORD_CATALOG, "app_unreachable", "x");
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        let v = std::fs::read_to_string(root.join("demo-video/record.json")).unwrap();
        assert!(v.contains("app_unreachable"), "{v}");
        let _ = std::fs::remove_dir_all(root);
    }
}
