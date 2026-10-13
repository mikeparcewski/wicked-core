//! Shadow mode for the step-0 seat floor (DES-PROC-GOVERNANCE §12, RECON-GOV-INVENTORY §6).
//!
//! The text fences (the gate hook, the ACP permission bridge, the command fences) stay
//! AUTHORITATIVE. The seat floor ([`crate::worker_sandbox::seat_floor`]) is armed underneath them,
//! and this module records, per governed unit, what each layer decided — so a later step can
//! delete a fence only once its shadow corpus shows zero "old denies, floor allows" cases.
//!
//! Three record kinds, one NDJSON file beside the unit's decisions log
//! (`<decisions dir>/floor-shadow.ndjson`), each line tagged with the unit's phase:
//!
//! - `arm`: what the floor was at spawn: the launcher (`sandbox-exec` | `bwrap` | `none`), the
//!   profile (`governed` | `read-only`), the roots, masks and network grants — or, on Windows, the
//!   label `no OS floor`. The armed spec is also written to `floor-spec-<phase>.json`, which the
//!   hook subprocess reads to judge each call against the same floor.
//! - `call`: one per tool call the old layer judged: the old verdict (`allow` | `deny`), the
//!   floor's verdict for the same call (`allow` | `deny` | `unknown` | `none`) with its basis,
//!   and the agreement class. The floor's verdict is a STATIC prediction from the call's paths
//!   (a path tool's path; a shell command's literal write targets) judged against the armed spec
//!   exactly as the profile renders it. A shell command whose targets the text cannot give is
//!   `unknown` — the kernel decides it at the syscall, which this record cannot see.
//! - `unit_summary`: the counts per agreement class, written when the unit ends.
//!
//! The agreement classes: `agree`; `old_denies_floor_allows` (the evidence a deletion step needs
//! to be zero); `floor_denies_old_allows` (an escape the old layer missed); `floor_unknown`;
//! `no_floor` (the unit ran with no floor: Windows).
//!
//! Recording is best effort and never changes a verdict: a write failure is reported on stderr.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::worker_sandbox::SeatFloor;

/// The record schema version.
pub(crate) const SHADOW_VERSION: u32 = 1;

/// The shadow log beside a decisions log.
pub(crate) fn shadow_path(decisions_path: &Path) -> PathBuf {
    decisions_path.with_file_name("floor-shadow.ndjson")
}

fn safe_phase(phase: &str) -> String {
    phase
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The armed floor spec for one unit (`phase`), beside its decisions log.
pub(crate) fn spec_path(decisions_path: &Path, phase: &str) -> PathBuf {
    decisions_path.with_file_name(format!("floor-spec-{}.json", safe_phase(phase)))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn append_line(path: &Path, v: &serde_json::Value) {
    let res = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(v).map_err(std::io::Error::other)?;
        line.push('\n');
        // ONE write of the whole line with O_APPEND: parallel hook processes never interleave.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(line.as_bytes())
    })();
    if let Err(e) = res {
        eprintln!(
            "wicked-core: floor shadow record not written to {}: {e}",
            path.display()
        );
    }
}

/// What the floor was for one governed unit at spawn.
#[derive(Debug, Clone)]
pub(crate) enum ArmOutcome<'a> {
    /// The floor armed with this launcher and spec.
    Armed {
        tool: &'static str,
        spec: &'a SeatFloor,
    },
    /// Windows, step 0: no floor, labelled [`crate::worker_sandbox::NO_OS_FLOOR`].
    NoOsFloor,
}

/// Record the floor a governed unit was armed with (`arm` line + the spec file the hook reads).
pub(crate) fn record_arm(
    decisions_path: &Path,
    phase: &str,
    carrier: &str,
    cli: &str,
    outcome: &ArmOutcome<'_>,
) {
    let line = match outcome {
        ArmOutcome::Armed { tool, spec } => {
            let spec_file = spec_path(decisions_path, phase);
            match serde_json::to_vec(spec) {
                Ok(bytes) => {
                    if let Some(parent) = spec_file.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(e) = std::fs::write(&spec_file, bytes) {
                        eprintln!(
                            "wicked-core: floor spec not written to {}: {e}",
                            spec_file.display()
                        );
                    }
                }
                Err(e) => eprintln!("wicked-core: floor spec not serialized: {e}"),
            }
            serde_json::json!({
                "v": SHADOW_VERSION, "kind": "arm", "phase": phase, "carrier": carrier,
                "cli": cli, "floor": tool, "label": null, "profile": spec.profile,
                "tree": spec.tree, "writable": spec.writable, "protected": spec.protected,
                "readmitted": spec.readmitted, "masks": spec.masks, "net": spec.net,
                "ts": now_ms(),
            })
        }
        ArmOutcome::NoOsFloor => {
            // `null`: a governed unit with NO floor (its calls read `no_floor`), as opposed to no
            // file at all (not a floor unit).
            let spec_file = spec_path(decisions_path, phase);
            if let Some(parent) = spec_file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&spec_file, b"null") {
                eprintln!(
                    "wicked-core: floor spec not written to {}: {e}",
                    spec_file.display()
                );
            }
            serde_json::json!({
                "v": SHADOW_VERSION, "kind": "arm", "phase": phase, "carrier": carrier,
                "cli": cli, "floor": "none", "label": crate::worker_sandbox::NO_OS_FLOOR,
                "platform": std::env::consts::OS, "ts": now_ms(),
            })
        }
    };
    append_line(&shadow_path(decisions_path), &line);
}

/// The floor recorded for `phase`: `None` when the unit recorded none (not a floor unit),
/// `Some(None)` for a governed unit with no floor (Windows), `Some(Some(spec))` when it armed.
pub(crate) fn load_recorded(decisions_path: &Path, phase: &str) -> Option<Option<SeatFloor>> {
    let bytes = std::fs::read(spec_path(decisions_path, phase)).ok()?;
    serde_json::from_slice::<Option<SeatFloor>>(&bytes).ok()
}

/// The armed spec for `phase`, if the unit's floor armed.
#[cfg(test)]
pub(crate) fn load_spec(decisions_path: &Path, phase: &str) -> Option<SeatFloor> {
    load_recorded(decisions_path, phase).flatten()
}

/// The floor's verdict on one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloorVerdict {
    Allow,
    Deny,
    /// The call's effect cannot be read from its text (the kernel decides at the syscall).
    Unknown,
    /// No floor armed (Windows).
    NoFloor,
}

impl FloorVerdict {
    fn as_str(self) -> &'static str {
        match self {
            FloorVerdict::Allow => "allow",
            FloorVerdict::Deny => "deny",
            FloorVerdict::Unknown => "unknown",
            FloorVerdict::NoFloor => "none",
        }
    }
}

/// One path a call touches, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) path: Option<PathBuf>,
    pub(crate) op: &'static str,
    pub(crate) spelled: String,
}

/// The floor's prediction for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prediction {
    pub(crate) verdict: FloorVerdict,
    pub(crate) basis: &'static str,
    pub(crate) targets: Vec<(Target, FloorVerdict)>,
}

/// `p` lexically normalized against `cwd` (`~` and `$HOME` expanded), then resolved where it
/// exists, so it compares with the spec's canonical roots.
fn normalize(p: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let expanded: PathBuf = if p == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home?.join(rest)
    } else {
        PathBuf::from(p)
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    // Resolve the nearest existing ancestor (macOS: `/var` is `/private/var`).
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = out.clone();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut r = c;
            for t in tail.iter().rev() {
                r.push(t);
            }
            return Some(r);
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(n), Some(parent)) => {
                tail.push(n);
                cur = parent.to_path_buf();
            }
            _ => return Some(out),
        }
    }
}

const WRITE_TOOLS: &[&str] = &[
    "write",
    "edit",
    "multiedit",
    "notebookedit",
    "str_replace_based_edit_tool",
];
const READ_TOOLS: &[&str] = &["read", "glob", "grep", "ls", "notebookread", "view"];
const SHELL_TOOLS: &[&str] = &["bash", "shell", "exec_command", "shell_command", "execute"];

fn path_arg(input: &serde_json::Value) -> Option<String> {
    for k in ["file_path", "filePath", "notebook_path", "path"] {
        if let Some(s) = input.get(k).and_then(serde_json::Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

fn command_arg(input: &serde_json::Value) -> Option<String> {
    for k in ["command", "cmd"] {
        match input.get(k) {
            Some(serde_json::Value::String(s)) => return Some(s.clone()),
            Some(serde_json::Value::Array(a)) => {
                let words: Vec<&str> = a.iter().filter_map(serde_json::Value::as_str).collect();
                // `["bash", "-lc", "<cmd>"]`: the command is the last word.
                if let Some(last) = words.last() {
                    return Some(last.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// The paths an `apply_patch` body adds, updates or deletes (codex's file tool).
fn patch_paths(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|l| {
            ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                .iter()
                .find_map(|p| l.strip_prefix(p))
        })
        .map(|p| p.trim().to_string())
        .collect()
}

/// The floor's prediction for `tool` with `input`, run in `cwd`, under `spec` (`None`: no floor).
pub(crate) fn predict(
    spec: Option<&SeatFloor>,
    tool: &str,
    input: &serde_json::Value,
    cwd: &Path,
    home: Option<&Path>,
) -> Prediction {
    let Some(spec) = spec else {
        return Prediction {
            verdict: FloorVerdict::NoFloor,
            basis: "no_floor",
            targets: Vec::new(),
        };
    };
    let name = tool.to_ascii_lowercase();
    let name = name.rsplit("__").next().unwrap_or(&name).to_string();
    let mut targets: Vec<Target> = Vec::new();
    let basis: &'static str;
    if WRITE_TOOLS.contains(&name.as_str()) || name == "apply_patch" {
        basis = "path";
        let spelled: Vec<String> = if name == "apply_patch" {
            let body = input
                .get("input")
                .or_else(|| input.get("patch"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| input.as_str())
                .unwrap_or_default();
            patch_paths(body)
        } else {
            path_arg(input).into_iter().collect()
        };
        for s in spelled {
            targets.push(Target {
                path: normalize(&s, cwd, home),
                op: "write",
                spelled: s,
            });
        }
    } else if READ_TOOLS.contains(&name.as_str()) {
        basis = "read_path";
        let s = path_arg(input).unwrap_or_else(|| ".".to_string());
        targets.push(Target {
            path: normalize(&s, cwd, home),
            op: "read",
            spelled: s,
        });
    } else if SHELL_TOOLS.contains(&name.as_str()) {
        let Some(cmd) = command_arg(input) else {
            return Prediction {
                verdict: FloorVerdict::Unknown,
                basis: "shell_unparsed",
                targets: Vec::new(),
            };
        };
        let words = crate::gate_hook::bash_write_targets(&cmd);
        if words.is_empty() {
            return Prediction {
                verdict: FloorVerdict::Unknown,
                basis: "shell_unparsed",
                targets: Vec::new(),
            };
        }
        basis = "shell_targets";
        for w in words {
            match crate::gate_hook::resolve_shell_readings(&w, home) {
                Some(readings) => {
                    for r in readings {
                        targets.push(Target {
                            path: normalize(&r, cwd, home),
                            op: "write",
                            spelled: w.clone(),
                        });
                    }
                }
                None => targets.push(Target {
                    path: None,
                    op: "write",
                    spelled: w,
                }),
            }
        }
    } else {
        return Prediction {
            verdict: FloorVerdict::Unknown,
            basis: "not_a_path_tool",
            targets: Vec::new(),
        };
    }
    let judged: Vec<(Target, FloorVerdict)> = targets
        .into_iter()
        .map(|t| {
            let v = match &t.path {
                None => FloorVerdict::Unknown,
                Some(p) if t.op == "read" => {
                    if spec.allows_read(p) {
                        FloorVerdict::Allow
                    } else {
                        FloorVerdict::Deny
                    }
                }
                Some(p) => {
                    if spec.allows_write(p) {
                        FloorVerdict::Allow
                    } else {
                        FloorVerdict::Deny
                    }
                }
            };
            (t, v)
        })
        .collect();
    // Deny dominates; then unknown; an empty target list (a write tool with no path) is unknown.
    let verdict = if judged.iter().any(|(_, v)| *v == FloorVerdict::Deny) {
        FloorVerdict::Deny
    } else if judged.is_empty() || judged.iter().any(|(_, v)| *v == FloorVerdict::Unknown) {
        FloorVerdict::Unknown
    } else {
        FloorVerdict::Allow
    };
    Prediction {
        verdict,
        basis,
        targets: judged,
    }
}

/// The agreement class of an old verdict (`old_allows`) and the floor's.
pub(crate) fn agreement(old_allows: bool, floor: FloorVerdict) -> &'static str {
    match (old_allows, floor) {
        (_, FloorVerdict::NoFloor) => "no_floor",
        (_, FloorVerdict::Unknown) => "floor_unknown",
        (true, FloorVerdict::Allow) | (false, FloorVerdict::Deny) => "agree",
        (false, FloorVerdict::Allow) => "old_denies_floor_allows",
        (true, FloorVerdict::Deny) => "floor_denies_old_allows",
    }
}

/// Record one judged tool call: the old layer's verdict (`old_allows`) beside the floor's.
/// Only for a unit whose floor was recorded at arm time (a spec, or `null` for `no OS floor`, read
/// as no floor). A call with neither is not a governed floor unit: nothing is recorded.
pub(crate) fn record_call(
    decisions_path: &Path,
    phase: &str,
    tool: &str,
    input: &serde_json::Value,
    cwd: &Path,
    home: Option<&Path>,
    old_allows: bool,
) {
    let shadow = shadow_path(decisions_path);
    let Some(spec) = load_recorded(decisions_path, phase) else {
        return;
    };
    let p = predict(spec.as_ref(), tool, input, cwd, home);
    let targets: Vec<serde_json::Value> = p
        .targets
        .iter()
        .map(|(t, v)| {
            serde_json::json!({
                "spelled": t.spelled, "path": t.path, "op": t.op, "floor": v.as_str(),
            })
        })
        .collect();
    let line = serde_json::json!({
        "v": SHADOW_VERSION, "kind": "call", "phase": phase, "tool": tool,
        "old": if old_allows { "allow" } else { "deny" },
        "floor": p.verdict.as_str(), "basis": p.basis, "targets": targets,
        "agreement": agreement(old_allows, p.verdict), "ts": now_ms(),
    });
    append_line(&shadow, &line);
}

/// The per-unit comparison: count `phase`'s `call` lines by agreement class and append a
/// `unit_summary` line. Nothing when the unit recorded no `arm` line.
pub(crate) fn summarize(decisions_path: &Path, phase: &str) -> Option<serde_json::Value> {
    let shadow = shadow_path(decisions_path);
    let text = std::fs::read_to_string(&shadow).ok()?;
    let mut armed = false;
    let mut counts: std::collections::BTreeMap<String, u64> = Default::default();
    let mut calls = 0u64;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("phase").and_then(serde_json::Value::as_str) != Some(phase) {
            continue;
        }
        match v.get("kind").and_then(serde_json::Value::as_str) {
            Some("arm") => armed = true,
            Some("call") => {
                calls += 1;
                let a = v
                    .get("agreement")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                *counts.entry(a.to_string()).or_default() += 1;
            }
            _ => {}
        }
    }
    if !armed {
        return None;
    }
    let get = |k: &str| counts.get(k).copied().unwrap_or(0);
    let line = serde_json::json!({
        "v": SHADOW_VERSION, "kind": "unit_summary", "phase": phase, "calls": calls,
        "agree": get("agree"),
        "old_denies_floor_allows": get("old_denies_floor_allows"),
        "floor_denies_old_allows": get("floor_denies_old_allows"),
        "floor_unknown": get("floor_unknown"),
        "no_floor": get("no_floor"),
        "ts": now_ms(),
    });
    append_line(&shadow, &line);
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(root: &Path) -> SeatFloor {
        SeatFloor {
            profile: "governed".into(),
            tree: root.join("wt"),
            writable: vec![root.join("wt"), root.join("home")],
            protected: Vec::new(),
            readmitted: Vec::new(),
            readmitted_files: Vec::new(),
            masks: vec![root.join("secret")],
            pinned: Vec::new(),
            net: Vec::new(),
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wicked-fsh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for s in ["wt", "home", "secret", "out"] {
            std::fs::create_dir_all(d.join(s)).unwrap();
        }
        d.canonicalize().unwrap()
    }

    #[test]
    fn a_path_tool_is_judged_against_the_spec_and_the_agreement_names_the_disagreement() {
        let d = tmp("path");
        let s = spec(&d);
        let cwd = d.join("wt");
        let inside = predict(
            Some(&s),
            "Write",
            &serde_json::json!({"file_path": "a.txt"}),
            &cwd,
            None,
        );
        assert_eq!(inside.verdict, FloorVerdict::Allow);
        let outside = predict(
            Some(&s),
            "Edit",
            &serde_json::json!({"file_path": d.join("out/x")}),
            &cwd,
            None,
        );
        assert_eq!(outside.verdict, FloorVerdict::Deny);
        let dotdot = predict(
            Some(&s),
            "Write",
            &serde_json::json!({"file_path": "../out/y"}),
            &cwd,
            None,
        );
        assert_eq!(dotdot.verdict, FloorVerdict::Deny, "{dotdot:?}");
        let secret = predict(
            Some(&s),
            "Read",
            &serde_json::json!({"file_path": d.join("secret/id_rsa")}),
            &cwd,
            None,
        );
        assert_eq!(secret.verdict, FloorVerdict::Deny);
        assert_eq!(
            agreement(false, FloorVerdict::Allow),
            "old_denies_floor_allows"
        );
        assert_eq!(
            agreement(true, FloorVerdict::Deny),
            "floor_denies_old_allows"
        );
        assert_eq!(agreement(true, FloorVerdict::Allow), "agree");
        assert_eq!(agreement(false, FloorVerdict::Unknown), "floor_unknown");
        assert_eq!(agreement(true, FloorVerdict::NoFloor), "no_floor");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_shell_call_is_judged_by_its_literal_targets_or_is_unknown() {
        let d = tmp("shell");
        let s = spec(&d);
        let cwd = d.join("wt");
        let out = d.join("out").join("f");
        let p = predict(
            Some(&s),
            "Bash",
            &serde_json::json!({"command": format!("echo x > {}", out.display())}),
            &cwd,
            None,
        );
        assert_eq!((p.verdict, p.basis), (FloorVerdict::Deny, "shell_targets"));
        let p = predict(
            Some(&s),
            "Bash",
            &serde_json::json!({"command": "echo x > in.txt"}),
            &cwd,
            None,
        );
        assert_eq!(p.verdict, FloorVerdict::Allow);
        let p = predict(
            Some(&s),
            "Bash",
            &serde_json::json!({"command": "python3 -c 'open(\"/x\",\"w\")'"}),
            &cwd,
            None,
        );
        assert_eq!(
            (p.verdict, p.basis),
            (FloorVerdict::Unknown, "shell_unparsed")
        );
        let p = predict(
            Some(&s),
            "Bash",
            &serde_json::json!({"command": "echo x > \"$X/f\""}),
            &cwd,
            None,
        );
        assert_eq!(p.verdict, FloorVerdict::Unknown, "{p:?}");
        let p = predict(
            None,
            "Bash",
            &serde_json::json!({"command": "true"}),
            &cwd,
            None,
        );
        assert_eq!(p.verdict, FloorVerdict::NoFloor);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The log format end to end: an `arm` line (+ spec file), a `call` line per judged call, and
    /// a `unit_summary` with the counts; a `no OS floor` arm writes no spec and calls read
    /// `no_floor`.
    #[test]
    fn the_shadow_log_records_arm_calls_and_a_unit_summary() {
        let d = tmp("log");
        let s = spec(&d);
        let decisions = d.join("gov").join("decisions.ndjson");
        record_arm(
            &decisions,
            "unit-1",
            "wrapped_cli",
            "claude",
            &ArmOutcome::Armed {
                tool: "sandbox-exec",
                spec: &s,
            },
        );
        assert_eq!(load_spec(&decisions, "unit-1").as_ref(), Some(&s));
        let cwd = d.join("wt");
        let outside = serde_json::json!({"file_path": d.join("out/x")});
        record_call(&decisions, "unit-1", "Write", &outside, &cwd, None, false);
        record_call(&decisions, "unit-1", "Write", &outside, &cwd, None, true);
        let inside = serde_json::json!({"file_path": "ok.txt"});
        record_call(&decisions, "unit-1", "Write", &inside, &cwd, None, false);
        let sum = summarize(&decisions, "unit-1").expect("armed unit has a summary");
        assert_eq!(sum["calls"], 3);
        assert_eq!(sum["agree"], 1);
        assert_eq!(sum["floor_denies_old_allows"], 1);
        assert_eq!(sum["old_denies_floor_allows"], 1);
        record_arm(&decisions, "unit-2", "acp", "pi", &ArmOutcome::NoOsFloor);
        assert!(load_spec(&decisions, "unit-2").is_none());
        record_call(&decisions, "unit-2", "Write", &inside, &cwd, None, true);
        let sum2 = summarize(&decisions, "unit-2").unwrap();
        assert_eq!(sum2["no_floor"], 1);
        let text = std::fs::read_to_string(shadow_path(&decisions)).unwrap();
        let arm2: serde_json::Value = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|v| v["kind"] == "arm" && v["phase"] == "unit-2")
            .unwrap();
        assert_eq!(arm2["label"], crate::worker_sandbox::NO_OS_FLOOR);
        assert_eq!(arm2["floor"], "none");
        assert!(
            summarize(&decisions, "unit-9").is_none(),
            "no arm, no summary"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
