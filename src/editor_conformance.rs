//! (DES-EDITOR-CONFORMANCE-HOST, e2e F18) The editor-plugin workflow's conformance step, run by the
//! engine on the host.
//!
//! An editor-plugin run's review could never pass: its `test` step told the review seat to run
//! garden's conformance harness, and no seat has a headless browser (`exit 3 … no headless browser`
//! on every attempt). The harness now runs as the engine's own Tool step, `conformance`
//! (catalog [`CATALOG_ID`]), between `build` and `test`:
//!
//! - **The command is fixed** ([`CONFORMANCE_CMD`]: garden's `conformance.py --engine`), run from the
//!   admitted skills generation, with a cleared environment, cwd = a private temp dir.
//! - **The jail** ([`crate::validator::serve_jail`]): writes only to that temp dir; network only on
//!   ONE loopback port, the harness's own origin (bind, inbound, outbound). Studio's host page comes
//!   from crew's studio dist, never from the daemon, so crew's API is unreachable from the browser
//!   that runs the seat-written editor. A host with no such jail (Windows) runs nothing:
//!   `unjailed_host`.
//! - **The inputs** are the engine's: the run's worktree (the ONE editor pack under it), the port, the
//!   out file, and two daemon variables crew exports at boot: [`PYTHON_ENV`] (its private Playwright
//!   venv's interpreter) and [`STUDIO_DIST_ENV`] (its studio dist). Nothing is installed at dispatch.
//! - **The record** ([`ConformanceRecord`]): the engine reads the report from the out file in its own
//!   temp dir, applies crew's required-case rule (cases 0-6 and 8-12 for every editor, plus 13-16
//!   when its `protocol` lists 2, crew `editors/pack-install.ts` `requiredCases`), and binds it to the
//!   worktree's tree id taken before and after the run (a tree that changed while it ran is refused).
//!
//! ## How it counts (coordinator ruling 1, 2026-11-06)
//!
//! The record is a FACT the review cites, and a hard gate on its own unit. It is NOT the run's QE
//! attributed PASS: QE acceptance stays a separate judgment of the editor's behaviour (conformance
//! covers the host contract; case 7 and what the editor does are out of its reach).
//!
//! - PASS: the unit is Done; the evaluator brief ([`crate::review_context::evaluator_brief`]) and the
//!   isolated QE reviewer's input carry [`brief_block`] as `[editor conformance record — unit N]`.
//! - A case failed, or the pack is invalid: Rejected, [`DENIAL_FAILED`]. A send-back goes to the
//!   creator (`build`) with the failed cases named.
//! - The harness could not run (no runtime, no studio dist, no jail, a port taken, the deadline, no
//!   report, a changed tree): Rejected, [`DENIAL_REFUSED`], in the rerun set: retry re-runs the Tool.

use serde::{Deserialize, Serialize};

/// The catalog id of the conformance step (neutral, engine-run Tool).
pub const CATALOG_ID: &str = "editor_conformance";
pub(crate) const CATALOG: &str = CATALOG_ID;
/// The Tool's fixed command: garden's conformance harness in engine mode.
pub const CONFORMANCE_CMD: [&str; 5] = [
    "wicked-garden",
    "run",
    "scripts/editor/conformance.py",
    "--engine",
    "--json",
];

/// `UnitDenial.source` of a run whose cases failed (or whose pack is invalid): a verdict on the
/// creator's work, so a send-back goes to `build`.
pub(crate) const DENIAL_FAILED: &str = "editor_conformance_failed";
/// `UnitDenial.source` of a run that could not happen: a command result the gate retries.
pub(crate) const DENIAL_REFUSED: &str = "editor_conformance_refused";

/// The daemon variable naming the interpreter with Playwright (crew's private venv). Crew exports it.
pub const PYTHON_ENV: &str = "WICKED_EDITOR_CONFORMANCE_PYTHON";
/// The daemon variable naming the studio dist the host page is served from. Crew exports it.
pub const STUDIO_DIST_ENV: &str = "WICKED_EDITOR_CONFORMANCE_STUDIO_DIST";

/// The longest one conformance run may take before the engine kills it.
pub(crate) const DEADLINE: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// The largest report the engine reads (garden bounds every string in an engine report; this is a backstop).
const REPORT_MAX: u64 = 8 * 1024 * 1024;

/// The causes garden's report gives that are a verdict on the pack (its author fixes it). Every
/// other cause kept the cases from running.
pub(crate) const VERDICT_CAUSES: &[&str] = &["case_failed", "pack_invalid"];

/// The cases every editor must pass (crew `CONFORMANCE_CASES`).
pub(crate) const CASES: [&str; 12] = [
    "0-manifest",
    "1-handshake",
    "2-sizes",
    "3-no-network",
    "4-permissions",
    "5-typing",
    "6-forwarding",
    "8-strict-input",
    "9-liveness",
    "10-keyboard",
    "11-theme",
    "12-hash-pinned",
];
/// The protocol-2 cases an editor whose `protocol` lists 2 must also pass (crew `CONFORMANCE_CASES_V2`).
pub(crate) const CASES_V2: [&str; 4] = ["13-surfaces", "14-card", "15-options", "16-full"];

/// The label prefix of the record block in a reviewer's context.
pub(crate) const RECORD_LABEL: &str = "[editor conformance record";

pub(crate) const OUTCOME_PASS: &str = "pass";
pub(crate) const OUTCOME_FAIL: &str = "fail";
pub(crate) const OUTCOME_REFUSED: &str = "refused";

/// One editor's line in the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorConformance {
    pub id: String,
    #[serde(default)]
    pub version: String,
    /// The sha256 of the entry bytes the cases ran against, as the harness hashed them.
    #[serde(default)]
    pub sha256: String,
    /// The protocols the manifest lists.
    #[serde(default)]
    pub protocol: Vec<u64>,
    /// Required cases that ran and failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// Required cases the report does not carry (they never ran).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

/// What the engine recorded for one conformance attempt, persisted on the conformance unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceRecord {
    /// [`OUTCOME_PASS`], [`OUTCOME_FAIL`] or [`OUTCOME_REFUSED`].
    pub outcome: String,
    /// The cause of a non-pass (garden's, or the engine's own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The worktree's tree id the cases ran on (the same before and after the run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The jail's launcher (`sandbox-exec`, `bwrap`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editors: Vec<EditorConformance>,
    pub attempt: u32,
}

impl ConformanceRecord {
    pub(crate) fn refused(attempt: u32, cause: &str, reason: impl Into<String>) -> Self {
        Self {
            outcome: OUTCOME_REFUSED.to_string(),
            cause: Some(cause.to_string()),
            reason: Some(reason.into()),
            tree: None,
            jail: None,
            editors: Vec::new(),
            attempt,
        }
    }

    pub(crate) fn passed(&self) -> bool {
        self.outcome == OUTCOME_PASS
    }
}

/// The required cases for an editor listing `protocol`.
pub(crate) fn required_cases(protocol: &[u64]) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = CASES.to_vec();
    if protocol.contains(&2) {
        v.extend(CASES_V2);
    }
    v
}

/// Judge garden's report (the out file's text, `None` when the Tool wrote none). The tree binding is
/// the caller's.
pub(crate) fn judge_report(raw: Option<&str>, attempt: u32) -> ConformanceRecord {
    let Some(raw) = raw else {
        return ConformanceRecord::refused(
            attempt,
            "no_report",
            "the conformance harness wrote no report",
        );
    };
    let report: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return ConformanceRecord::refused(
                attempt,
                "no_report",
                format!("the conformance report is not JSON ({e})"),
            )
        }
    };
    let s = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let mut editors = Vec::new();
    for e in report
        .get("editors")
        .and_then(|e| e.as_array())
        .into_iter()
        .flatten()
    {
        let protocol: Vec<u64> = match e.get("protocol") {
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|p| p.as_u64()).collect(),
            Some(p) => p.as_u64().into_iter().collect(),
            None => Vec::new(),
        };
        let cases = e.get("cases").and_then(|c| c.as_object());
        let mut failed = Vec::new();
        let mut missing = Vec::new();
        for case in required_cases(&protocol) {
            match cases.and_then(|c| c.get(case)) {
                None => missing.push(case.to_string()),
                Some(c) if c.get("ok").and_then(|o| o.as_bool()) == Some(true) => {}
                Some(_) => failed.push(case.to_string()),
            }
        }
        // A case the harness ran beyond the required set and failed (e.g. 17) is still a failure.
        if let Some(c) = cases {
            for (k, v) in c {
                if v.get("ok").and_then(|o| o.as_bool()) != Some(true)
                    && !failed.iter().any(|f| f == k)
                {
                    failed.push(k.clone());
                }
            }
        }
        editors.push(EditorConformance {
            id: s(e, "id").unwrap_or_default(),
            version: s(e, "version").unwrap_or_default(),
            sha256: s(e, "sha256").unwrap_or_default(),
            protocol,
            failed,
            missing,
        });
    }
    let ok = report.get("ok").and_then(|o| o.as_bool()) == Some(true);
    let cause = s(&report, "cause");
    let error = s(&report, "error");
    let base = |outcome: &str, cause: Option<String>, reason: Option<String>| ConformanceRecord {
        outcome: outcome.to_string(),
        cause,
        reason,
        tree: None,
        jail: None,
        editors: editors.clone(),
        attempt,
    };
    let any_failed = editors.iter().any(|e| !e.failed.is_empty());
    let any_missing = editors.iter().any(|e| !e.missing.is_empty());
    if ok {
        if editors.is_empty() {
            return base(
                OUTCOME_REFUSED,
                Some("no_editors".into()),
                Some("the report passed but names no editor".into()),
            );
        }
        if any_failed {
            return base(
                OUTCOME_FAIL,
                Some("case_failed".into()),
                Some(failed_summary(&editors)),
            );
        }
        if any_missing {
            // Not run is not failed: the host page could not run them (e.g. a protocol-1 studio).
            return base(
                OUTCOME_REFUSED,
                Some("cases_not_run".into()),
                Some(format!(
                    "the conformance host did not run every required case ({}); crew refuses such an \
                     install too",
                    missing_summary(&editors)
                )),
            );
        }
        return base(OUTCOME_PASS, None, None);
    }
    match cause.as_deref() {
        Some(c) if VERDICT_CAUSES.contains(&c) => {
            let reason = if any_failed {
                failed_summary(&editors)
            } else {
                error.unwrap_or_else(|| c.to_string())
            };
            base(OUTCOME_FAIL, cause, Some(reason))
        }
        Some(_) => base(OUTCOME_REFUSED, cause, error),
        None => base(
            OUTCOME_REFUSED,
            Some("no_cause".into()),
            Some(format!(
                "the report failed with no cause (a wicked-garden before engine mode?): {}",
                error.unwrap_or_default()
            )),
        ),
    }
}

fn failed_summary(editors: &[EditorConformance]) -> String {
    editors
        .iter()
        .filter(|e| !e.failed.is_empty())
        .map(|e| format!("{}: {} failed", e.id, e.failed.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

fn missing_summary(editors: &[EditorConformance]) -> String {
    editors
        .iter()
        .filter(|e| !e.missing.is_empty())
        .map(|e| format!("{}: {} not run", e.id, e.missing.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Read the report the Tool wrote (no link followed, at most [`REPORT_MAX`] bytes): `Ok(None)` when it
/// wrote none, `Err` when it is not one the engine reads (too large, not a plain file).
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

/// Judge the report file the Tool wrote.
pub(crate) fn judge_file(path: &std::path::Path, attempt: u32) -> ConformanceRecord {
    match read_report(path) {
        Ok(raw) => judge_report(raw.as_deref(), attempt),
        Err(why) => ConformanceRecord::refused(attempt, "bad_report", why),
    }
}

/// The host inputs crew exports, resolved outside the jail: `(python, studio dist)`, each an
/// absolute existing path, or the refusal `(cause, reason)`.
pub(crate) fn host_inputs() -> Result<(std::path::PathBuf, std::path::PathBuf), (String, String)> {
    let abs = |k: &str| {
        std::env::var_os(k)
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let python = abs(PYTHON_ENV).filter(|p| p.is_file()).ok_or_else(|| {
        (
            "runtime_missing".to_string(),
            format!(
                "this daemon names no conformance browser runtime (${PYTHON_ENV} unset or not a file \
                 yet). wicked-crew provisions its private Playwright venv when an editor-plugin run \
                 launches (a few minutes the first time); retry this step once it is ready"
            ),
        )
    })?;
    let dist = abs(STUDIO_DIST_ENV)
        .filter(|p| p.join("index.html").is_file())
        .ok_or_else(|| {
            (
                "no_studio".to_string(),
                format!(
                    "this daemon names no studio dist for the conformance host page (${STUDIO_DIST_ENV} \
                     unset, or no index.html in it); wicked-crew exports its own bundled studio at boot"
                ),
            )
        })?;
    Ok((python, dist))
}

/// The host variables the harness needs beside the declared ones: the browser cache and home.
pub(crate) fn host_passthrough_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::var_os("WG_EDITOR_CONFORMANCE_TIMEOUT")
        .map(|v| {
            (
                "WG_EDITOR_CONFORMANCE_TIMEOUT".to_string(),
                v.to_string_lossy().into_owned(),
            )
        })
        .into_iter()
        .collect();
    // Path variables only when absolute: the harness runs from its own temp dir, so a relative one would
    // name another place than the one crew provisioned under (codex, crew review r3).
    for k in [
        "PLAYWRIGHT_BROWSERS_PATH",
        "WICKED_GARDEN_CACHE_DIR",
        "XDG_CACHE_HOME",
        "HOME",
    ] {
        if let Some(v) = std::env::var_os(k).filter(|v| std::path::Path::new(v).is_absolute()) {
            env.push((k.to_string(), v.to_string_lossy().into_owned()));
        }
    }
    env
}

/// The declared variables of one run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tool_env(
    run_id: &str,
    ord: u32,
    worktree: &std::path::Path,
    port: u16,
    out: &std::path::Path,
    python: &std::path::Path,
    dist: &std::path::Path,
    jail: &str,
) -> Vec<(String, String)> {
    let p = |x: &std::path::Path| x.to_string_lossy().into_owned();
    vec![
        ("WICKED_RUN_ID".to_string(), run_id.to_string()),
        ("WICKED_RUN_UNIT".to_string(), ord.to_string()),
        (
            "WICKED_EDITOR_CONFORMANCE_JAIL".to_string(),
            jail.to_string(),
        ),
        ("WICKED_EDITOR_PACK_ROOT".to_string(), p(worktree)),
        ("WG_EDITOR_CONFORMANCE_PORT".to_string(), port.to_string()),
        ("WG_EDITOR_CONFORMANCE_OUT".to_string(), p(out)),
        ("WG_EDITOR_CONFORMANCE_PYTHON".to_string(), p(python)),
        ("WG_EDITOR_CONFORMANCE_STUDIO_DIST".to_string(), p(dist)),
    ]
}

/// A loopback port free right now (the jail opens it; a race with another process that takes it
/// first is a `port_unavailable` refusal, and retry picks another).
pub(crate) fn free_port() -> std::io::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// The record as a reviewer reads it: what the engine ran, where, on which tree, and what it found.
/// `current` is the tree under review now: a PASS on any other tree (or when the current one could not
/// be read) is not evidence for it.
pub(crate) fn brief_block(
    ord: u32,
    after: Option<u32>,
    r: &ConformanceRecord,
    current: Option<&str>,
) -> String {
    let tree = r
        .tree
        .as_deref()
        .map(|t| format!(", tree {t}"))
        .unwrap_or_default();
    let jail = r
        .jail
        .as_deref()
        .map(|j| format!(" in its jail ({j}, network only to the harness's own loopback port)"))
        .unwrap_or_default();
    let after = after
        .map(|a| format!(" after unit {a}"))
        .unwrap_or_default();
    let editors = r
        .editors
        .iter()
        .map(|e| {
            let p = if e.protocol.contains(&2) {
                "cases 0-6, 8-16"
            } else {
                "cases 0-6, 8-12"
            };
            format!("{} {} (entry sha256 {}; {p})", e.id, e.version, e.sha256)
        })
        .collect::<Vec<_>>()
        .join("; ");
    let head = format!(
        "The engine ran the editor conformance cases itself on this host{after}{jail} (unit {ord}{tree}): "
    );
    let stale = r.passed() && (current.is_none() || r.tree.as_deref() != current);
    let body = match r.outcome.as_str() {
        OUTCOME_PASS if stale => format!(
            "PASSED, but on another tree than the one under review ({}): the work changed after this \
             run, so conformance is UNVERIFIED for this tree. A conformance item is a FAIL until the \
             engine runs the cases again (a send-back to the creator re-runs them).",
            current.unwrap_or("the current tree could not be read")
        ),
        OUTCOME_PASS => format!(
            "PASSED — every required case for every editor: {editors}. This is the conformance \
             evidence for this tree: cite it for any conformance item. Do not run conformance.py in \
             your seat (no seat has a headless browser; an in-seat exit 3 is not a finding)."
        ),
        OUTCOME_FAIL => format!(
            "FAILED — {}. A conformance item is a FAIL naming these cases.",
            r.reason.as_deref().unwrap_or("a required case failed")
        ),
        _ => format!(
            "COULD NOT RUN ({}: {}). The operator went past it: conformance is UNVERIFIED for this \
             tree; never report it as passed.",
            r.cause.as_deref().unwrap_or("refused"),
            r.reason.as_deref().unwrap_or("")
        ),
    };
    head + &body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(editors: serde_json::Value, ok: bool, cause: Option<&str>) -> String {
        let mut v = serde_json::json!({"ok": ok, "pack": "/t/p", "editors": editors});
        if let Some(c) = cause {
            v["cause"] = c.into();
        }
        v.to_string()
    }

    fn cases(names: &[&str], failing: &[&str]) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for n in names {
            m.insert(
                n.to_string(),
                serde_json::json!({"ok": !failing.contains(n)}),
            );
        }
        serde_json::Value::Object(m)
    }

    fn all_v2() -> Vec<&'static str> {
        required_cases(&[1, 2])
    }

    #[test]
    fn a_full_report_passes_and_a_protocol_2_editor_needs_13_to_16() {
        let e = serde_json::json!([{"id": "acme-x", "version": "0.1.0", "sha256": "ab", "protocol": [1, 2],
            "cases": cases(&all_v2(), &[])}]);
        let r = judge_report(Some(&report(e, true, None)), 0);
        assert!(r.passed(), "{r:?}");
        let short =
            serde_json::json!([{"id": "acme-x", "protocol": [1, 2], "cases": cases(&CASES, &[])}]);
        let r = judge_report(Some(&report(short, true, None)), 0);
        assert_eq!(
            (r.outcome.as_str(), r.cause.as_deref()),
            (OUTCOME_REFUSED, Some("cases_not_run"))
        );
        let v1 =
            serde_json::json!([{"id": "acme-x", "protocol": [1], "cases": cases(&CASES, &[])}]);
        assert!(judge_report(Some(&report(v1, true, None)), 0).passed());
    }

    #[test]
    fn a_failed_case_is_a_verdict_even_when_the_report_says_ok() {
        let e = serde_json::json!([{"id": "acme-x", "protocol": [1, 2], "cases": cases(&all_v2(), &["14-card"])}]);
        let r = judge_report(Some(&report(e.clone(), true, None)), 0);
        assert_eq!(r.outcome, OUTCOME_FAIL);
        assert_eq!(r.editors[0].failed, vec!["14-card".to_string()]);
        let r = judge_report(Some(&report(e, false, Some("case_failed"))), 0);
        assert_eq!(
            (r.outcome.as_str(), r.reason.as_deref()),
            (OUTCOME_FAIL, Some("acme-x: 14-card failed"))
        );
    }

    #[test]
    fn host_causes_are_refusals_and_pack_causes_are_verdicts() {
        for c in [
            "no_browser",
            "no_jail",
            "port_unavailable",
            "no_studio",
            "bad_arguments",
        ] {
            let r = judge_report(Some(&report(serde_json::json!([]), false, Some(c))), 0);
            assert_eq!(r.outcome, OUTCOME_REFUSED, "{c}");
        }
        let r = judge_report(
            Some(&report(serde_json::json!([]), false, Some("pack_invalid"))),
            0,
        );
        assert_eq!(r.outcome, OUTCOME_FAIL);
        // No report, garbage, an ok report naming no editor, a failure with no cause: refusals.
        assert_eq!(judge_report(None, 0).outcome, OUTCOME_REFUSED);
        assert_eq!(judge_report(Some("not json"), 0).outcome, OUTCOME_REFUSED);
        assert_eq!(
            judge_report(Some(&report(serde_json::json!([]), true, None)), 0).outcome,
            OUTCOME_REFUSED
        );
        assert_eq!(
            judge_report(Some(&report(serde_json::json!([]), false, None)), 0)
                .cause
                .as_deref(),
            Some("no_cause")
        );
    }

    #[test]
    fn the_brief_block_says_what_ran_where_and_never_asks_for_an_in_seat_run() {
        let e = serde_json::json!([{"id": "acme-x", "version": "0.1.0", "sha256": "ab", "protocol": [1, 2],
            "cases": cases(&all_v2(), &[])}]);
        let mut r = judge_report(Some(&report(e, true, None)), 1);
        r.tree = Some("t1".into());
        r.jail = Some("sandbox-exec".into());
        let b = brief_block(6, Some(5), &r, Some("t1"));
        assert!(b.contains("after unit 5"), "{b}");
        assert!(b.contains("tree t1") && b.contains("PASSED"), "{b}");
        assert!(b.contains("Do not run conformance.py in your seat"), "{b}");
        assert!(!b.contains("UNVERIFIED"), "{b}");
        // A PASS on another tree, or when the current tree is unknown, is not evidence for this one.
        for current in [Some("t2"), None] {
            let b = brief_block(6, Some(5), &r, current);
            assert!(b.contains("UNVERIFIED") && b.contains("is a FAIL"), "{b}");
        }
        let refused = ConformanceRecord::refused(0, "runtime_missing", "x");
        assert!(brief_block(6, None, &refused, Some("t1")).contains("UNVERIFIED"));
    }

    #[test]
    fn crews_required_cases_are_mirrored() {
        // crew packages/crew/src/editors/pack-install.ts CONFORMANCE_CASES / CONFORMANCE_CASES_V2.
        assert_eq!(
            CASES.join(","),
            "0-manifest,1-handshake,2-sizes,3-no-network,4-permissions,5-typing,6-forwarding,\
             8-strict-input,9-liveness,10-keyboard,11-theme,12-hash-pinned"
        );
        assert_eq!(CASES_V2.join(","), "13-surfaces,14-card,15-options,16-full");
    }

    /// The REAL harness in the REAL jail (the build step DES §2(c) left to prove: Chromium under the jail's
    /// deny-writes profile, serving and driving its own port). Opt-in, because it needs garden, a studio dist
    /// and a Playwright interpreter: `WICKED_LIVE_EDITOR_CONFORMANCE=<garden root>` with this module's two
    /// daemon variables set and `WICKED_LIVE_EDITOR_PACK_TREE` naming a tree with one scaffolded editor pack.
    #[test]
    fn the_real_harness_passes_in_the_serve_jail_when_asked() {
        let Some(garden) = std::env::var_os("WICKED_LIVE_EDITOR_CONFORMANCE") else {
            return;
        };
        let garden = std::path::PathBuf::from(garden);
        let (python, dist) = host_inputs().expect("the two daemon variables");
        let tree = std::env::var_os("WICKED_LIVE_EDITOR_PACK_TREE")
            .map(std::path::PathBuf::from)
            .expect("WICKED_LIVE_EDITOR_PACK_TREE");
        let tmp = crate::repo_checks::PrivateTmp::create().unwrap();
        let port = free_port().unwrap();
        let jail = crate::validator::serve_jail(&[tmp.path()], port);
        assert_eq!(jail.level, crate::validator::SandboxLevel::Sandboxed);
        let out = tmp.path().join("report.json");
        let mut cmd = std::process::Command::new(&jail.wrapper[0]);
        cmd.args(&jail.wrapper[1..])
            .arg("python3")
            .arg(garden.join("scripts/editor/conformance.py"))
            .args(["--engine", "--json"])
            .current_dir(tmp.path());
        crate::validator::apply_minimal_env(&mut cmd);
        let t = tmp.path().to_string_lossy().into_owned();
        for k in ["TMPDIR", "TMP", "TEMP"] {
            cmd.env(k, &t);
        }
        cmd.envs(host_passthrough_env());
        cmd.envs(tool_env(
            "live",
            1,
            &tree,
            port,
            &out,
            &python,
            &dist,
            "sandbox-exec",
        ));
        let status = cmd.status().expect("the jail runs");
        let rec = judge_file(&out, 0);
        assert!(rec.passed(), "status {status:?}: {rec:?}");
    }

    /// The cases run on the floor's own guarded checkout of the recorded tree (`repo_checks::
    /// checkout_guarded_tree`): tracked and untracked-not-ignored files are there, an ignored one is not, and
    /// a later worktree edit changes nothing.
    #[cfg(unix)]
    #[test]
    fn the_cases_run_on_the_floors_checkout_of_the_recorded_tree() {
        let base = std::env::temp_dir().join(format!("ec-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join("p/editors")).unwrap();
        let git = |args: &[&str]| {
            // spawn-audit: test-only — a git fixture for the checkout under test.
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(repo.join("p/wicked-pack.json"), "{}").unwrap();
        std::fs::write(repo.join("p/editors/index.html"), "<p>one</p>").unwrap();
        std::fs::write(repo.join("ignored.txt"), "no").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(repo.join("untracked.txt"), "kept").unwrap();
        let snap = crate::worktree_guard::snapshot(&repo, &repo).unwrap();
        let git_dir = std::path::PathBuf::from(snap.git_dir.clone().unwrap());
        std::fs::write(repo.join("p/editors/index.html"), "<p>two</p>").unwrap();
        let co = crate::repo_checks::checkout_guarded_tree(&repo, &git_dir, &snap.tree, "ec-test")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(co.dir.join("p/editors/index.html")).unwrap(),
            "<p>one</p>"
        );
        assert!(co.dir.join("untracked.txt").is_file());
        assert!(!co.dir.join("ignored.txt").exists());
        drop(co);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_report_is_not_read() {
        let d = std::env::temp_dir().join(format!("ec-link-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("real.json"), "{}").unwrap();
        std::os::unix::fs::symlink(d.join("real.json"), d.join("report.json")).unwrap();
        assert!(read_report(&d.join("report.json")).is_err());
        assert_eq!(
            judge_file(&d.join("report.json"), 0).cause.as_deref(),
            Some("bad_report")
        );
        assert_eq!(
            read_report(&d.join("real.json")).unwrap().as_deref(),
            Some("{}")
        );
        assert_eq!(read_report(&d.join("absent.json")).unwrap(), None);
        let _ = std::fs::remove_dir_all(d);
    }
}
