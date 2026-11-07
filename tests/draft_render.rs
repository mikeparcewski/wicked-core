//! (PLAN G12) The draft render step through the REAL engine.
//!
//! A draft with a page budget had no measured page count anywhere in its run, so its review stalled
//! (garden's `page_count.py` exits 3 in a seat: no seat has a browser). The engine now prints the draft
//! itself, as the `draft_render` Tool, and the review is handed the record.
//!
//! A repo-less plan `draft → measure-pages → review` (crew's launch shape: the draft is the launch's
//! declared deliverable in a declared write root) dispatches the catalog's fixed command (a stub
//! `wicked-garden` FIRST on `PATH`, which answers by the mode written in the draft itself) in the
//! render jail:
//!
//! - a verified print completes the run with a `measured` record (the count, the sha256 of the draft's
//!   bytes) and the review's context carries `[draft render record — unit N]` and the rule never to
//!   render in the seat. Without the fix there is no such step and no record: this fails;
//! - a print that could not happen (the counter's own estimate, a report that is not its own) is
//!   `not_measured`, and the run still completes: the step never holds it;
//! - a send-back to the draft re-runs the render on the new bytes (a new sha256, the next attempt).
//!
//! POSIX only: the stub is a shell script (the suite's idiom).

#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;
use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, PlanSteps, SandboxLevel,
    StepInput, StepOutput, StepRunner, StepStatus, TeamConfig, TEAM_OUTBOX_FILE,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join(format!("wicked-core-dr-{}", std::process::id()))
}

/// Pre-main (single-threaded): the hermetic emit spool, a stub `wicked-garden` first on `PATH`, a
/// fixture renderer file named by `WICKED_CHROME`, and an empty claude config dir (no garden ladder).
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches the filesystem and
/// process env vars via the std API.
#[ctor::ctor(unsafe)]
fn arm() {
    wicked_apps_core::emit::hermetic_test_spool();
    let root = fixture_root();
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("fixture bin dir");
    let stub = bin.join("wicked-garden");
    // The last argument is the draft; its text names the mode. Every line to stderr is the Tool's
    // output; the report goes to stdout (the engine sends it to its report file).
    let script = "#!/bin/sh\n\
         echo \"ARGV $*\" >&2\n\
         env | sed 's/^/ENV /' >&2\n\
         for a in \"$@\"; do f=\"$a\"; done\n\
         if grep -q MODE:MEASURED \"$f\"; then n=$(grep -o 'PAGES:[0-9]*' \"$f\" | cut -d: -f2); \
           printf '{\"check\":\"pages\",\"pages\":%s,\"source\":\"chrome-render\",\"verified\":true,\"notes\":[],\"estimate\":{\"estimate\":1}}' \"$n\"; exit 0; fi\n\
         if grep -q MODE:UNVERIFIED \"$f\"; then \
           printf '%s' '{\"check\":\"pages\",\"pages\":2,\"source\":\"html-estimate\",\"verified\":false,\"notes\":[\"chrome exited 1 without writing a PDF: no display\"],\"estimate\":{\"estimate\":2}}'; exit 3; fi\n\
         echo 'not a report'\n\
         exit 0\n";
    std::fs::write(&stub, script).expect("write the stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let chrome = root.join("headless-shell");
    std::fs::write(&chrome, "#!/bin/sh\nexit 0\n").expect("fixture renderer");
    let config = root.join("claude-config");
    std::fs::create_dir_all(&config).expect("fixture claude config dir");
    std::env::remove_var("WICKED_SKILLS_SNAPSHOT");
    std::env::set_var("CLAUDE_CONFIG_DIR", &config);
    std::env::set_var(wicked_core::draft_render::CHROME_ENV, &chrome);
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::set_var("PATH", std::env::join_paths(paths).expect("PATH joins"));
}

struct NoBallots;
impl Dispatcher for NoBallots {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "test".into(),
        })
    }
}

/// Each run's review context: `(label, text)` pairs, by run id; and the draft text by run id.
type Seen = Mutex<HashMap<String, Vec<(String, String)>>>;

fn seen() -> &'static Seen {
    static SEEN: OnceLock<Seen> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// Each run's draft file and the text its draft writes on each attempt.
type Drafts = Mutex<HashMap<String, (PathBuf, Vec<String>)>>;

fn drafts() -> &'static Drafts {
    static D: OnceLock<Drafts> = OnceLock::new();
    D.get_or_init(Default::default)
}

/// The seats: `draft` (the creator) writes the run's next draft text to the declared file; `review`
/// records its context and passes. Every other agent unit answers plainly.
struct Seats;
impl StepRunner for Seats {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let out = |output: String| StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        };
        if i.unit.session_id == "validator" {
            return out(wicked_core::stub_rule_verdicts(&i.unit.description)
                .unwrap_or_else(|| "PASS\nthe work is there\nPASS".into()));
        }
        if i.unit.id.ends_with(":draft") {
            let (path, texts) = drafts().lock().unwrap().get(&i.run_id).cloned().unwrap();
            let t = texts
                .get(i.attempt as usize)
                .or(texts.last())
                .cloned()
                .unwrap();
            std::fs::write(&path, t).unwrap();
            return out(format!(
                "Outline: one section. Wrote the complete draft to {}",
                path.display()
            ));
        }
        if i.unit.id.ends_with(":review") {
            seen().lock().unwrap().insert(
                i.run_id.clone(),
                i.prior_outputs
                    .iter()
                    .map(|p| (p.label.clone(), p.output.clone()))
                    .collect(),
            );
            // A three-page draft of a one-page brief is sent back (the send-back test's first round).
            let (path, _) = drafts().lock().unwrap().get(&i.run_id).cloned().unwrap();
            if std::fs::read_to_string(path)
                .unwrap_or_default()
                .contains("PAGES:3")
            {
                return out(
                    "ITEM 1: FAIL — 3 printed pages for a one-page brief\nVERDICT: FAIL".into(),
                );
            }
            return out("ITEM 1: PASS — the draft is there\nVERDICT: PASS".into());
        }
        out("RISK {\"band\": 10, \"why\": \"a routine internal draft\"}\nThe ask is clear.".into())
    }
}

fn cli(key: &str) -> AgenticCli {
    AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: "unused".into(),
        headless_invocation: "unused {PROMPT}".into(),
        category: Category::default(),
        input_mode: InputMode::default(),
        version_probe: vec![],
        trust_flags: vec![],
        alt_binaries: vec![],
        confidence: Confidence::default(),
        enabled_for_council: true,
        seat_eligible_for_work: true,
        acp: None,
        capabilities: None,
        login_invocation: None,
        logout_invocation: None,
        governance_class: None,
        credential: None,
        free_tier: None,
        health: None,
    }
}

fn dir(name: &str) -> PathBuf {
    let d = fixture_root().join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Run {
    sid: String,
    core: Core,
    rx: std::sync::mpsc::Receiver<CoreEvent>,
    events: Vec<CoreEvent>,
    draft: PathBuf,
}

/// Launch `draft → measure-pages → review` repo-less, with the draft as the launch's declared
/// deliverable in a declared write root (crew's shape), and collect events until the run stops.
/// `texts[n]` is what the draft writes on attempt n.
fn run(name: &str, texts: &[&str]) -> Run {
    let base = dir(name);
    let inbox = base.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let draft = inbox.join(format!("{name}-v1.html"));
    let core = Core::spawn_with_engine_team(
        base.join("estate.db").to_string_lossy().into_owned(),
        Arc::new(NoBallots),
        Arc::new(Seats),
        TeamConfig::new(None, Some(base.join(TEAM_OUTBOX_FILE)))
            .with_final_pass_budget(Duration::from_millis(300)),
    );
    let rx = core.subscribe();
    let sid = format!("dr-{name}");
    drafts().lock().unwrap().insert(
        sid.clone(),
        (draft.clone(), texts.iter().map(|t| t.to_string()).collect()),
    );
    let plan: PlanSteps = serde_json::from_value(json!({
        "steps": [
            {"catalog": "produce", "id": "draft"},
            {"catalog": "draft_render", "id": "measure-pages", "depends_on": ["draft"]},
            {"catalog": "critique", "id": "review", "depends_on": ["measure-pages"]}
        ]
    }))
    .unwrap();
    core.launch_run(LaunchSpec {
        problem: "draft a one-page brief".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: sid.clone(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        base_ref: None,
        workflow: None,
        project_id: None,
        extra_write_roots: vec![inbox.to_string_lossy().into_owned()],
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: Some(plan),
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: vec![draft.to_string_lossy().into_owned()],
        qe_acceptance: Default::default(),
    })
    .expect("the draft plan launches");
    let mut r = Run {
        sid,
        core,
        rx,
        events: Vec::new(),
        draft,
    };
    r.until_stopped();
    for _ in 0..3 {
        let plan_gate = r.events.iter().rev().find_map(|e| match e {
            CoreEvent::AwaitingHuman { gate_kind, .. } => Some(gate_kind == "plan_approval"),
            CoreEvent::SessionCompleted { .. } | CoreEvent::GateEscalated { .. } => Some(false),
            _ => None,
        });
        if plan_gate != Some(true) {
            break;
        }
        r.answer(HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        });
    }
    r
}

impl Run {
    /// Collect events until the run completes, fails or pauses.
    fn until_stopped(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(500)) {
                Ok(ev) => {
                    let done = match &ev {
                        CoreEvent::SessionCompleted { session } => *session == self.sid,
                        CoreEvent::SessionFailed { session, .. } => *session == self.sid,
                        CoreEvent::AwaitingHuman { session, .. } => *session == self.sid,
                        _ => false,
                    };
                    self.events.push(ev);
                    if done {
                        return;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return,
            }
        }
    }

    /// Answer the gate, then collect until the run stops again.
    fn answer(&mut self, d: HumanDecision) {
        self.core
            .confirm_gate(&self.sid, d)
            .expect("the gate takes the answer");
        self.until_stopped();
    }

    /// How many times the plan's step `step` was dispatched (the floor may add steps before it).
    fn dispatches_of(&self, step: &str) -> usize {
        let ord = self
            .events
            .iter()
            .find_map(|e| match e {
                CoreEvent::UnitPlanned {
                    ord, description, ..
                } if description.starts_with(&format!("{step} — ")) => Some(*ord),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no step {step} planned"));
        self.dispatches(ord)
    }

    /// How many times unit `ord` was dispatched.
    fn dispatches(&self, ord: u32) -> usize {
        self.events
            .iter()
            .filter(|e| {
                matches!(e, CoreEvent::UnitDispatched { session, ord: o, .. } if *session == self.sid && *o == ord)
            })
            .count()
    }
}

fn types(events: &[CoreEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| e.to_json()["type"].as_str().unwrap_or("?").to_string())
        .collect()
}

fn completed(events: &[CoreEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, CoreEvent::SessionCompleted { .. }))
}

/// The render unit's record, from the run's units.
fn record(r: &Run) -> wicked_core::draft_render::DraftRenderRecord {
    let views = r.core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == r.sid).unwrap();
    v.units
        .iter()
        .find(|u| u.catalog.as_deref() == Some("draft_render"))
        .and_then(|u| u.draft_render.clone())
        .unwrap_or_else(|| panic!("no render record: {:?}", types(&r.events)))
}

fn review_ctx(r: &Run) -> Vec<(String, String)> {
    seen()
        .lock()
        .unwrap()
        .get(&r.sid)
        .cloned()
        .expect("the review ran")
}

/// Whether this host has no render jail (Windows, Linux without a usable bwrap): the step records
/// `unjailed_host` and the run still completes; the one condition under which these tests stop early.
fn unjailed(r: &Run) -> bool {
    let rec = record(r);
    if rec.cause.as_deref() == Some("unjailed_host") {
        assert!(completed(&r.events), "{:?}", types(&r.events));
        assert!(
            !(cfg!(target_os = "macos")
                && wicked_core::sandbox_availability().0 == SandboxLevel::Sandboxed),
            "a macOS host with sandbox-exec must run the step: {rec:?}"
        );
        eprintln!("SKIP: no render jail on this host: {rec:?}");
        return true;
    }
    false
}

/// THE G12 test: the engine prints the draft, and the review is handed the count.
#[test]
fn a_measured_draft_completes_and_the_review_cites_the_engines_count() {
    let text = "<!doctype html><p>MODE:MEASURED PAGES:2</p>";
    let r = run("measured", &[text]);
    if unjailed(&r) {
        return;
    }
    assert!(completed(&r.events), "{:?}", types(&r.events));
    let rec = record(&r);
    assert!(rec.measured(), "{rec:?}");
    assert_eq!(rec.pages, Some(2));
    assert_eq!(rec.estimate, Some(1));
    assert_eq!(
        rec.sha256.as_deref(),
        Some(
            format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes())
            )
            .as_str()
        ),
        "the sha256 of the raw bytes the run wrote (crew's floor key)"
    );
    assert_eq!(rec.file.as_deref(), Some(r.draft.to_str().unwrap()));
    assert!(rec.producer.is_some(), "{rec:?}");
    assert!(rec.jail.is_some() && rec.renderer.is_some() && rec.counter.is_some());
    // The fixed command on the draft itself, with only the declared variables.
    let out = r
        .core
        .work_output(&format!("{}:measure-pages", r.sid))
        .expect("the Tool's output");
    assert!(out.contains("measured 2 printed pages"), "{out}");
    let ctx = review_ctx(&r);
    let (label, text) = ctx
        .iter()
        .find(|(l, _)| l.starts_with("[draft render record"))
        .unwrap_or_else(|| panic!("no render record in the review's context: {ctx:?}"));
    assert!(label.starts_with("[draft render record — unit "), "{label}");
    assert!(
        text.contains("2 printed pages") && text.contains("judge any page budget against it"),
        "{text}"
    );
    let checklist = ctx
        .iter()
        .find(|(l, _)| l.starts_with("[review checklist"))
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    assert!(checklist.contains("Never run a renderer"), "{checklist}");
}

/// A print that could not happen is recorded as such (never a count), and never holds the run.
#[test]
fn an_unmeasured_draft_is_recorded_honestly_and_the_run_completes() {
    for (name, text, cause) in [
        ("unverified", "<p>MODE:UNVERIFIED</p>", "render_failed"),
        ("badreport", "<p>MODE:NONE</p>", "bad_report"),
    ] {
        let r = run(name, &[text]);
        if unjailed(&r) {
            return;
        }
        assert!(completed(&r.events), "{name}: {:?}", types(&r.events));
        let rec = record(&r);
        assert_eq!(rec.outcome, "not_measured", "{name}: {rec:?}");
        assert_eq!(rec.cause.as_deref(), Some(cause), "{name}: {rec:?}");
        assert_eq!(rec.pages, None, "{name}");
        assert!(rec.sha256.is_some(), "{name}: the bytes read are named");
        let ctx = review_ctx(&r);
        let (_, t) = ctx
            .iter()
            .find(|(l, _)| l.starts_with("[draft render record"))
            .unwrap();
        assert!(
            t.contains("NOT MEASURED") && t.contains("DEFERRED"),
            "{name}: {t}"
        );
        if name == "unverified" {
            assert!(t.contains("no display"), "{t}");
            assert!(t.contains("lower bound, not a count"), "{t}");
        }
    }
}

/// A send-back to the draft resets the render: it runs again on the new bytes, with its own attempt,
/// and the review is handed the new count.
#[test]
fn a_send_back_re_measures_the_reworked_draft() {
    let v1 = "<p>MODE:MEASURED PAGES:3</p>";
    let v2 = "<p>MODE:MEASURED PAGES:1</p>";
    let mut r = run("sendback", &[v1, v2]);
    if unjailed(&r) {
        return;
    }
    assert!(
        !completed(&r.events),
        "the review failed the 3-page draft: {:?}",
        types(&r.events)
    );
    let first = record(&r);
    assert_eq!(first.pages, Some(3), "{first:?}");
    r.answer(HumanDecision::RequestChanges {
        note: Some("cut it to one page".into()),
    });
    assert!(completed(&r.events), "{:?}", types(&r.events));
    assert_eq!(
        (r.dispatches_of("draft"), r.dispatches_of("measure-pages")),
        (2, 2),
        "{:?}",
        types(&r.events)
    );
    let second = record(&r);
    assert_eq!(second.pages, Some(1), "{second:?}");
    assert_ne!(second.sha256, first.sha256);
    assert!(
        second.attempt >= 1,
        "the re-run is its own attempt: {second:?}"
    );
    let ctx = review_ctx(&r);
    let (_, t) = ctx
        .iter()
        .find(|(l, _)| l.starts_with("[draft render record"))
        .unwrap();
    assert!(
        t.contains("1 printed page ") && !t.contains("NOT evidence"),
        "{t}"
    );
}
