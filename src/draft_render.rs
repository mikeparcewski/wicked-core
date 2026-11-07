//! (PLAN G12, operator decision D4) The draft's page count, measured by the engine on the host.
//!
//! A draft with a page budget had no measured page count anywhere in its run: the review seat was
//! told to run garden's `self_check.py … --render`, no seat has a browser, so `page_count.py` exited
//! 3 and the review stalled. The `interactive-draft` preset now runs `draft → measure-pages →
//! critique`, where `measure-pages` is this catalog Tool ([`CATALOG_ID`]), on the pattern of
//! [`crate::editor_conformance`] (a host-run step, not a seat):
//!
//! - **The file** is the last `.html`/`.htm` deliverable the most recent creator declared (the
//!   launch's deliverables are appended to it last), resolved by the deliverable floor's rule
//!   ([`resolve_draft`]) and read once, no link followed, at most [`DRAFT_MAX`] bytes
//!   ([`read_draft`]). The counter prints the draft at its own path (the URL crew's floor prints
//!   too), and the engine reads it again after the print: the record's sha256 is of bytes that were
//!   the same before and after, else nothing is measured (`draft_changed`).
//! - **The counter** is garden's `page_count.py --render --json` ([`RENDER_CMD`]), the module
//!   crew's floor counts with, run from the admitted skills generation in [`crate::validator::render_jail`]
//!   (no network at all, writes only to the temp dir). Its stdout goes to a report file the engine
//!   reads ([`judge_report`]).
//! - **The renderer** is the daemon's [`CHROME_ENV`] only (crew provisions a headless shell and exports
//!   it). Unset: nothing is spawned and the record says `no_renderer`.
//!
//! ## How it counts
//!
//! The record ([`DraftRenderRecord`]) is a FACT the reviewer is handed ([`brief_block`]), never a
//! gate: no measurement outcome denies the unit, and "not measured" is recorded as such, with its
//! cause, never as a count. Crew's publication floor reuses a `measured` record when its own sha256
//! of the file (the raw bytes the run wrote, before interactive instruments them) and [`PROFILE`]
//! match; otherwise it renders (or estimates) itself.

use serde::{Deserialize, Serialize};
use sha2::Digest;

/// The catalog id of the render step (neutral, engine-run Tool).
pub const CATALOG_ID: &str = "draft_render";
/// The Tool's fixed command: garden's page counter, printing the draft with the renderer. The engine
/// appends the draft's real path.
pub const RENDER_CMD: [&str; 5] = [
    "wicked-garden",
    "run",
    "scripts/draft/page_count.py",
    "--render",
    "--json",
];
/// The daemon variable naming the browser the counter prints with (crew exports its managed one).
pub const CHROME_ENV: &str = "WICKED_CHROME";
/// What a `measured` count means, for a consumer that reuses it: the counter, the renderer's print
/// settings (garden `page_count.py`'s `render_pdf`), and where the draft was printed from.
pub const PROFILE: &str = "page_count.py --render: headless --print-to-pdf, no header/footer, \
                           offline host rules, 5 s virtual time; the draft at its own path";

/// The label prefix of the record block in a reviewer's context.
pub(crate) const RECORD_LABEL: &str = "[draft render record";

pub const OUTCOME_MEASURED: &str = "measured";
pub const OUTCOME_NOT_MEASURED: &str = "not_measured";

/// The longest a render may take before the engine kills it (the counter's own browser timeout is 90 s).
pub(crate) const DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
/// The largest draft the engine reads.
pub(crate) const DRAFT_MAX: u64 = 32 * 1024 * 1024;
/// The largest report the engine reads.
const REPORT_MAX: u64 = 1024 * 1024;

/// What the engine recorded for one render attempt, persisted on the render unit (`WorkUnit::draft_render`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftRenderRecord {
    /// [`OUTCOME_MEASURED`] or [`OUTCOME_NOT_MEASURED`].
    pub outcome: String,
    /// Why it was not measured: `no_renderer`, `no_draft`, `no_garden`, `unjailed_host`,
    /// `render_failed`, `bad_report`, `draft_changed`, `deadline_exceeded`, `engine_refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The ord of the creator unit whose deliverable this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<u32>,
    /// The deliverable, as that creator declared it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Lowercase hex sha256 of the exact bytes measured (whenever the file was read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// The printed page count. ONLY when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    /// The counter's structural estimate: a lower bound, never a count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<u32>,
    /// The browser it printed with (when measured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renderer: Option<String>,
    /// [`PROFILE`] (when measured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Where the counter ran from: the skills generation's root, or `PATH` (when measured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counter: Option<String>,
    /// The jail's launcher (`sandbox-exec`, `bwrap`), when the counter ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jail: Option<String>,
    pub attempt: u32,
}

impl DraftRenderRecord {
    pub(crate) fn not_measured(attempt: u32, cause: &str, reason: impl Into<String>) -> Self {
        Self {
            outcome: OUTCOME_NOT_MEASURED.to_string(),
            cause: Some(cause.to_string()),
            reason: Some(reason.into()),
            producer: None,
            file: None,
            sha256: None,
            bytes: None,
            pages: None,
            estimate: None,
            renderer: None,
            profile: None,
            counter: None,
            jail: None,
            attempt,
        }
    }

    pub fn measured(&self) -> bool {
        self.outcome == OUTCOME_MEASURED && self.pages.is_some()
    }

    /// The one line the unit's output carries (never the counter's raw output).
    pub(crate) fn summary(&self) -> String {
        match (self.measured(), self.pages) {
            (true, Some(n)) => format!(
                "{CATALOG_ID}: measured {n} printed page{} ({}, sha256 {})",
                if n == 1 { "" } else { "s" },
                self.file.as_deref().unwrap_or("the draft"),
                self.sha256.as_deref().unwrap_or("?")
            ),
            _ => format!(
                "{CATALOG_ID}: not measured ({}): {}",
                self.cause.as_deref().unwrap_or("unknown"),
                self.reason.as_deref().unwrap_or("")
            ),
        }
    }
}

/// Lowercase hex sha256.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn is_html(declared: &str) -> bool {
    let l = declared.to_ascii_lowercase();
    l.ends_with(".html") || l.ends_with(".htm")
}

/// A resolved draft: the path as declared, the real root it lies in, and its real path below it.
#[derive(Debug, Clone)]
pub(crate) struct Draft {
    pub declared: String,
    pub root: std::path::PathBuf,
    pub real: std::path::PathBuf,
}

/// The draft among `declared` (a creator's `required_deliverables`): the LAST `.html`/`.htm` entry,
/// resolved by the deliverable floor's rule ([`crate::path_policy::missing_deliverables`]): an
/// absolute path only when its real path lies inside a declared write root; a relative one under
/// `cwd`, then each write root, never through `..`, and only when its real path stays inside the base
/// it was joined to. `Ok(Draft)`, or why there is none.
pub(crate) fn resolve_draft(
    declared: &[String],
    cwd: &std::path::Path,
    write_roots: &[String],
) -> Result<Draft, String> {
    let Some(d) = declared.iter().rev().find(|d| is_html(d.trim())) else {
        return Err("the draft step declared no HTML deliverable".into());
    };
    let p = std::path::Path::new(d.trim());
    let found = if p.is_absolute() {
        p.canonicalize().ok().and_then(|real| {
            write_roots.iter().find_map(|r| {
                std::path::Path::new(r)
                    .canonicalize()
                    .ok()
                    .filter(|root| real.starts_with(root))
                    .map(|root| (root, real.clone()))
            })
        })
    } else if p
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        None
    } else {
        std::iter::once(cwd.to_path_buf())
            .chain(write_roots.iter().map(std::path::PathBuf::from))
            .find_map(|base| {
                let root = base.canonicalize().ok()?;
                base.join(p)
                    .canonicalize()
                    .ok()
                    .filter(|real| real.starts_with(&root))
                    .map(|real| (root, real))
            })
    };
    found
        .map(|(root, real)| Draft {
            declared: d.clone(),
            root,
            real,
        })
        .ok_or_else(|| {
        format!("the draft {d} is not a file inside the run's declared write roots or its working directory")
    })
}

/// Open `real` by walking down from `root` one component at a time, each `openat` with `O_NOFOLLOW`
/// on the directory just opened: no link anywhere below the root is followed, so a directory swapped
/// for a link after [`resolve_draft`] cannot lead the read out of the root.
#[cfg(unix)]
fn open_beneath(root: &std::path::Path, real: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let rel = real
        .strip_prefix(root)
        .map_err(|_| "the draft is not below its root".to_string())?;
    let comps: Vec<&std::ffi::OsStr> = rel
        .components()
        .map(|c| match c {
            std::path::Component::Normal(n) => Ok(n),
            _ => Err("the draft's path is not plain below its root".to_string()),
        })
        .collect::<Result<_, _>>()?;
    if comps.is_empty() {
        return Err("the draft is its root".into());
    }
    let cstr =
        |b: &[u8]| std::ffi::CString::new(b).map_err(|_| "a NUL in the draft's path".to_string());
    let root_c = cstr(root.as_os_str().as_bytes())?;
    // SAFETY: plain libc calls on NUL-terminated paths; every fd opened here is closed or handed to
    // exactly one `File`.
    let mut fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "the draft's root could not be opened: {}",
            std::io::Error::last_os_error()
        ));
    }
    for (i, c) in comps.iter().enumerate() {
        let last = i + 1 == comps.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if last {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        let name = match cstr(c.as_bytes()) {
            Ok(n) => n,
            Err(e) => {
                unsafe { libc::close(fd) };
                return Err(e);
            }
        };
        let next = unsafe { libc::openat(fd, name.as_ptr(), flags) };
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        if next < 0 {
            return Err(format!("the draft could not be opened: {err}"));
        }
        fd = next;
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn open_beneath(_root: &std::path::Path, real: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::File::open(real).map_err(|e| format!("the draft could not be opened: {e}"))
}

/// Read the resolved draft: opened beneath its root with no link followed ([`open_beneath`]), a
/// regular file, at most [`DRAFT_MAX`] bytes.
pub(crate) fn read_draft(d: &Draft) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = open_beneath(&d.root, &d.real)?;
    let meta = f
        .metadata()
        .map_err(|e| format!("the draft could not be read: {e}"))?;
    if !meta.is_file() {
        return Err("the draft is not a regular file".into());
    }
    let mut buf = Vec::new();
    f.take(DRAFT_MAX + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("the draft could not be read: {e}"))?;
    if buf.len() as u64 > DRAFT_MAX {
        return Err(format!("the draft is over {DRAFT_MAX} bytes"));
    }
    Ok(buf)
}

/// The sha256 of the draft a creator declared, as it is NOW (the reviewer's comparison), or `None`
/// when it cannot be resolved or read.
pub(crate) fn current_sha256(
    declared: &[String],
    cwd: &std::path::Path,
    write_roots: &[String],
) -> Option<String> {
    let d = resolve_draft(declared, cwd, write_roots).ok()?;
    read_draft(&d).ok().map(|b| sha256_hex(&b))
}

/// The renderer the daemon names in [`CHROME_ENV`]: an absolute path to an existing file, or the
/// `(cause, reason)` of a render that cannot happen here.
pub(crate) fn renderer() -> Result<std::path::PathBuf, (String, String)> {
    renderer_from(std::env::var_os(CHROME_ENV))
}

/// [`renderer`] over a given value of the variable.
fn renderer_from(v: Option<std::ffi::OsString>) -> Result<std::path::PathBuf, (String, String)> {
    v.map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_file())
        .ok_or_else(|| {
            (
                "no_renderer".to_string(),
                format!(
                    "this daemon names no renderer (${CHROME_ENV} unset, relative or not a file); \
                     wicked-crew provisions a headless browser when a document run launches"
                ),
            )
        })
}

/// Read the report file the counter's stdout went to (no link followed, at most [`REPORT_MAX`]).
pub(crate) fn read_report(path: &std::path::Path) -> Result<Option<String>, String> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(None);
    };
    if !meta.file_type().is_file() {
        return Err("the report is not a plain file".into());
    }
    if meta.len() > REPORT_MAX {
        return Err(format!("the report is over {REPORT_MAX} bytes"));
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|e| format!("the report could not be read: {e}"))
}

/// Judge the counter's report into `rec` (which already carries the file, its hash and the attempt).
/// `measured` needs all of: the command exited 0, the report is ONE JSON object with
/// `check == "pages"`, `verified == true`, `source == "chrome-render"` and an integer `pages` ≥ 1.
pub(crate) fn judge_report(
    mut rec: DraftRenderRecord,
    raw: Result<Option<String>, String>,
    exited_ok: bool,
    renderer: &std::path::Path,
) -> DraftRenderRecord {
    let fail = |mut r: DraftRenderRecord, cause: &str, why: String| {
        r.outcome = OUTCOME_NOT_MEASURED.to_string();
        r.cause = Some(cause.to_string());
        r.reason = Some(why);
        r.pages = None;
        r
    };
    let raw = match raw {
        Ok(Some(t)) => t,
        Ok(None) => return fail(rec, "bad_report", "the page counter wrote no report".into()),
        Err(why) => return fail(rec, "bad_report", why),
    };
    let report: serde_json::Value = match serde_json::from_str(raw.trim()) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        Ok(_) => return fail(rec, "bad_report", "the report is not a JSON object".into()),
        Err(e) => {
            return fail(
                rec,
                "bad_report",
                format!("the report is not one JSON object ({e})"),
            )
        }
    };
    if report.get("check").and_then(|c| c.as_str()) != Some("pages") {
        return fail(
            rec,
            "bad_report",
            "the report is not the page counter's (no check: pages)".into(),
        );
    }
    rec.estimate = report
        .pointer("/estimate/estimate")
        .and_then(|e| e.as_u64())
        .and_then(|e| u32::try_from(e).ok());
    let verified = report.get("verified").and_then(|v| v.as_bool()) == Some(true);
    let source = report.get("source").and_then(|v| v.as_str());
    let pages = report
        .get("pages")
        .and_then(|p| p.as_u64())
        .filter(|p| *p >= 1)
        .and_then(|p| u32::try_from(p).ok());
    match (exited_ok, verified, source, pages) {
        (true, true, Some("chrome-render"), Some(n)) => {
            rec.outcome = OUTCOME_MEASURED.to_string();
            rec.cause = None;
            rec.reason = None;
            rec.pages = Some(n);
            rec.renderer = Some(renderer.to_string_lossy().into_owned());
            rec.profile = Some(PROFILE.to_string());
            rec
        }
        _ => {
            let notes = report
                .get("notes")
                .and_then(|n| n.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .filter(|x| !x.starts_with("UNVERIFIED"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let why = if notes.is_empty() {
                "the renderer printed no page count".to_string()
            } else {
                notes.chars().take(600).collect()
            };
            fail(rec, "render_failed", why)
        }
    }
}

/// The record's block in the reviewer's brief. `current` is the sha256 of the draft under review now
/// (resolved from the reviewer's creator), `None` when it could not be read.
pub(crate) fn brief_block(ord: u32, r: &DraftRenderRecord, current: Option<&str>) -> String {
    let file = r.file.as_deref().unwrap_or("the draft");
    let defer = "A page-budget checklist item is DEFERRED — \"the page count the host measures \
                 before the document lands\": never PASS it and never FAIL it for the missing \
                 measurement (a draft whose structure plainly breaks the budget is still a FAIL). \
                 Never state a page count.";
    let head = format!("The engine printed the draft itself on this host (unit {ord}, {file}): ");
    let body = match (r.measured(), r.pages) {
        (true, Some(n)) if current.is_some() && r.sha256.as_deref() == current => format!(
            "{n} printed page{} (measured with a headless browser, sha256 {}). This is the \
             draft's page count: judge any page budget against it.",
            if n == 1 { "" } else { "s" },
            r.sha256.as_deref().unwrap_or("")
        ),
        (true, Some(n)) => format!(
            "{n} printed pages for sha256 {}, but the draft under review is {}: the count is NOT \
             evidence for it. {defer}",
            r.sha256.as_deref().unwrap_or("?"),
            current.unwrap_or("unreadable now")
        ),
        _ => {
            let est = r
                .estimate
                .map(|e| format!(" Its structural estimate, {e}, is a lower bound, not a count."))
                .unwrap_or_default();
            format!(
                "NOT MEASURED ({}: {}).{est} {defer}",
                r.cause.as_deref().unwrap_or("unknown"),
                r.reason.as_deref().unwrap_or("")
            )
        }
    };
    head + &body
}

/// The checklist rule handed beside a record.
pub(crate) const NO_SEAT_RENDER_RULE: &str = "\n- [draft render record] is the engine's own print of \
     the draft on this host: cite it for any page-count item. Never run a renderer, `page_count.py \
     --render` or `self_check.py --render` in your seat (no seat has a browser), and its absence there \
     is not a finding.";

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> DraftRenderRecord {
        let mut r = DraftRenderRecord::not_measured(0, "engine_refused", "x");
        r.file = Some("/inbox/doc-v1.html".into());
        r.sha256 = Some("ab".into());
        r
    }

    fn report(v: serde_json::Value) -> Result<Option<String>, String> {
        Ok(Some(v.to_string()))
    }

    #[test]
    fn a_verified_render_is_measured_and_anything_else_is_not() {
        let chrome = std::path::Path::new("/c/headless");
        let ok = serde_json::json!({"check":"pages","pages":2,"source":"chrome-render","verified":true,
            "estimate":{"estimate":1}});
        let r = judge_report(base(), report(ok.clone()), true, chrome);
        assert!(r.measured(), "{r:?}");
        assert_eq!((r.pages, r.estimate), (Some(2), Some(1)));
        assert_eq!(r.renderer.as_deref(), Some("/c/headless"));
        assert_eq!(r.profile.as_deref(), Some(PROFILE));
        assert!(r.cause.is_none() && r.reason.is_none());
        // A non-zero exit never measures, whatever the report says.
        assert!(!judge_report(base(), report(ok), false, chrome).measured());
        // The counter's estimate (no browser) is not a count.
        let est = serde_json::json!({"check":"pages","pages":2,"source":"html-estimate","verified":false,
            "notes":["chrome exited 1 without writing a PDF: boom","UNVERIFIED — structural estimate only"],
            "estimate":{"estimate":2}});
        let r = judge_report(base(), report(est), false, chrome);
        assert_eq!(r.outcome, OUTCOME_NOT_MEASURED);
        assert_eq!(r.cause.as_deref(), Some("render_failed"));
        assert_eq!(r.pages, None);
        assert_eq!(r.estimate, Some(2));
        assert!(r.reason.as_deref().unwrap().contains("boom"), "{r:?}");
        // Another program's JSON, two objects, a zero count, nothing: not measured.
        for raw in [
            report(serde_json::json!({"pages":2,"verified":true,"source":"chrome-render"})),
            Ok(Some("{\"check\":\"pages\"}\n{\"check\":\"pages\"}".into())),
            report(
                serde_json::json!({"check":"pages","pages":0,"source":"chrome-render","verified":true}),
            ),
            Ok(None),
            Err("the report is over 1 bytes".into()),
        ] {
            let r = judge_report(base(), raw, true, chrome);
            assert!(!r.measured() && r.pages.is_none(), "{r:?}");
        }
    }

    #[test]
    fn the_draft_resolves_only_inside_its_declared_roots() {
        let d = std::env::temp_dir().join(format!("wc-draft-render-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (inbox, cwd, out) = (d.join("inbox"), d.join("cwd"), d.join("outside"));
        for x in [&inbox, &cwd, &out] {
            std::fs::create_dir_all(x).unwrap();
        }
        std::fs::write(inbox.join("doc-v1.html"), "<p>a</p>").unwrap();
        std::fs::write(out.join("secret.html"), "<p>s</p>").unwrap();
        std::fs::write(cwd.join("rel.html"), "<p>r</p>").unwrap();
        let roots = vec![inbox.to_string_lossy().into_owned()];
        let abs = inbox.join("doc-v1.html").to_string_lossy().into_owned();
        // The LAST html entry wins; non-html entries are skipped.
        let dr = resolve_draft(
            &[
                "notes.json".into(),
                "/x/early.html".into(),
                abs.clone(),
                "a.json".into(),
            ],
            &cwd,
            &roots,
        )
        .unwrap();
        assert_eq!(dr.declared, abs);
        assert_eq!(read_draft(&dr).unwrap(), b"<p>a</p>");
        // Absolute outside every root, `..`, and none at all: refused.
        let outside = out.join("secret.html").to_string_lossy().into_owned();
        assert!(resolve_draft(&[outside], &cwd, &roots).is_err());
        assert!(resolve_draft(&["../outside/secret.html".into()], &cwd, &roots).is_err());
        assert!(resolve_draft(&["notes.json".into()], &cwd, &roots).is_err());
        // Relative: the cwd, then the roots.
        assert!(resolve_draft(&["rel.html".into()], &cwd, &roots).is_ok());
        assert!(resolve_draft(&["doc-v1.html".into()], &cwd, &roots).is_ok());
        #[cfg(unix)]
        {
            // A link in the inbox that points out of it resolves outside: refused.
            std::os::unix::fs::symlink(out.join("secret.html"), inbox.join("link.html")).unwrap();
            let l = inbox.join("link.html").to_string_lossy().into_owned();
            assert!(resolve_draft(std::slice::from_ref(&l), &cwd, &roots).is_err());
            // The read itself follows no link below its root: a directory swapped for a link
            // after the resolve does not lead it out.
            std::fs::create_dir_all(inbox.join("sub")).unwrap();
            std::fs::write(inbox.join("sub/d.html"), "<p>d</p>").unwrap();
            let sub = resolve_draft(
                &[inbox.join("sub/d.html").to_string_lossy().into_owned()],
                &cwd,
                &roots,
            )
            .unwrap();
            std::fs::create_dir_all(out.join("sub")).unwrap();
            std::fs::write(out.join("sub/d.html"), "<p>secret</p>").unwrap();
            std::fs::remove_dir_all(inbox.join("sub")).unwrap();
            std::os::unix::fs::symlink(out.join("sub"), inbox.join("sub")).unwrap();
            assert!(
                read_draft(&sub).is_err(),
                "a swapped parent is never followed"
            );
            let leaf = Draft {
                declared: l,
                root: sub.root.clone(),
                real: inbox.canonicalize().unwrap().join("link.html"),
            };
            assert!(read_draft(&leaf).is_err(), "a final link is never followed");
        }
        // A directory is not a draft.
        std::fs::create_dir_all(inbox.join("dir.html")).unwrap();
        let dir = resolve_draft(
            &[inbox.join("dir.html").to_string_lossy().into_owned()],
            &cwd,
            &roots,
        )
        .unwrap();
        assert!(read_draft(&dir).is_err());
        assert_eq!(
            current_sha256(&[abs], &cwd, &roots).as_deref(),
            Some(sha256_hex(b"<p>a</p>").as_str())
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_brief_block_says_measured_stale_or_not_measured() {
        let mut m = base();
        m.outcome = OUTCOME_MEASURED.into();
        m.cause = None;
        m.reason = None;
        m.pages = Some(1);
        let b = brief_block(2, &m, Some("ab"));
        assert!(
            b.contains("1 printed page ") && b.contains("judge any page budget against it"),
            "{b}"
        );
        assert!(!b.contains("DEFERRED"), "{b}");
        let stale = brief_block(2, &m, Some("cd"));
        assert!(
            stale.contains("NOT evidence") && stale.contains("DEFERRED"),
            "{stale}"
        );
        let unreadable = brief_block(2, &m, None);
        assert!(unreadable.contains("NOT evidence"), "{unreadable}");
        let mut n = DraftRenderRecord::not_measured(0, "no_renderer", "no browser here");
        n.estimate = Some(2);
        let b = brief_block(2, &n, Some("ab"));
        assert!(
            b.contains("NOT MEASURED (no_renderer: no browser here)"),
            "{b}"
        );
        assert!(
            b.contains("lower bound, not a count") && b.contains("DEFERRED"),
            "{b}"
        );
        assert!(b.contains("Never state a page count"), "{b}");
    }

    #[test]
    fn no_renderer_is_named_when_the_variable_is_unset_relative_or_missing() {
        for v in [
            None,
            Some("headless-shell"),
            Some("/definitely/not/a/browser"),
        ] {
            let (cause, _) = renderer_from(v.map(Into::into)).unwrap_err();
            assert_eq!(cause, "no_renderer", "{v:?}");
        }
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            renderer_from(Some(exe.clone().into_os_string())).unwrap(),
            exe
        );
    }

    #[test]
    fn the_record_skips_what_it_does_not_have_on_the_wire() {
        let r = DraftRenderRecord::not_measured(1, "no_renderer", "x");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"outcome":"not_measured","cause":"no_renderer","reason":"x","attempt":1})
        );
    }
}
