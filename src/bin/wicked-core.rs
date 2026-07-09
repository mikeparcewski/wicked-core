//! Thin operator CLI over the COE library — the replacement entry point for the retired
//! `wicked-agent` binary. All composition lives in `wicked_core`; this is just argv + printing.
//!
//!   wicked-core status                            # list sessions + units on the store
//!   wicked-core repos                             # list registered repositories
//!   wicked-core register-repo --path <dir> [--name N]   # register a git repo to run within
//!   wicked-core run --problem "Do X. Do Y" \      # interactive governed run (streams events)
//!       [--repo <id>] [--confirm none|all|before:N] [--session <id>]
//!   wicked-core resume --session <id>             # resume a paused/interrupted run
//!   wicked-core cancel --session <id>             # cancel a run
//!   wicked-core launch --problem "..."            # legacy straight-through run (no gates)
//!   wicked-core gate-hook --scope S --phase P     # PreToolUse governance hook (claude invokes this)
//!   [--db <path>]                                 # else $WICKED_ESTATE_DB, else ./wicked-estate.db

use std::io::BufRead;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wicked_core::{
    registry_roster, run_gate_hook, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision,
    LaunchSpec, RepoSpec,
};

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn store_path(args: &[String]) -> String {
    flag(args, "--db")
        .or_else(|| {
            std::env::var("WICKED_ESTATE_DB")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "wicked-estate.db".to_string())
}

/// Parse `--confirm none|all|before:N` into a [`HumanConfirm`] policy (default `None`).
fn parse_confirm(args: &[String]) -> HumanConfirm {
    match flag(args, "--confirm").as_deref() {
        Some("all") => HumanConfirm::All,
        Some(s) if s.starts_with("before:") => s[7..]
            .parse::<u32>()
            .map(HumanConfirm::Before)
            .unwrap_or(HumanConfirm::None),
        _ => HumanConfirm::None,
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // gate-hook runs as a SUBPROCESS that claude spawns per tool-call. It must NOT spawn the actor
    // (it never writes the store — it only reads policies and appends decisions.ndjson), so handle
    // it before `Core::spawn` and exit with the gate's code (2 = deny ⇒ claude aborts the call).
    if args.get(1).map(String::as_str) == Some("gate-hook") {
        let scope = flag(&args, "--scope").unwrap_or_default();
        let phase = flag(&args, "--phase").unwrap_or_default();
        let db = flag(&args, "--db");
        std::process::exit(run_gate_hook(&scope, &phase, db.as_deref()));
    }

    let core = Core::spawn(store_path(&args));

    match args.get(1).map(String::as_str) {
        Some("status") => print_status(&core),
        Some("repos") => match core.list_repos() {
            Ok(rs) if rs.is_empty() => println!("(no repos registered)"),
            Ok(rs) => {
                for r in rs {
                    println!("{}  {}  [{}]", r.id, r.root_path, r.default_branch);
                }
            }
            Err(e) => fail(&format!("repos failed: {e}")),
        },
        Some("register-repo") => {
            let Some(path) = flag(&args, "--path") else {
                fail("register-repo requires --path <dir>");
                return;
            };
            let name = flag(&args, "--name").unwrap_or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "repo".to_string())
            });
            match core.register_repo(RepoSpec {
                name,
                root_path: path,
                registered_at: now_secs(),
            }) {
                Ok(e) => println!(
                    "registered {} → {} [{}]",
                    e.id, e.root_path, e.default_branch
                ),
                Err(e) => fail(&format!("register-repo failed: {e}")),
            }
        }
        Some("run") => run_interactive(&core, &args),
        Some("resume") => {
            let Some(sid) = flag(&args, "--session") else {
                fail("resume requires --session <id>");
                return;
            };
            match core.resume_run(&sid) {
                Ok(s) => println!("resumed {sid} → {s:?}"),
                Err(e) => fail(&format!("resume failed: {e}")),
            }
        }
        Some("cancel") => {
            let Some(sid) = flag(&args, "--session") else {
                fail("cancel requires --session <id>");
                return;
            };
            match core.cancel_run(&sid) {
                Ok(s) => println!("cancelled {sid} → {s:?}"),
                Err(e) => fail(&format!("cancel failed: {e}")),
            }
        }
        Some("launch") => {
            let Some(problem) = flag(&args, "--problem") else {
                fail("launch requires --problem \"...\"");
                return;
            };
            let events = core.subscribe();
            let sid = core.launch(LaunchSpec {
                problem,
                clis: registry_roster(),
                entity_mode: EntityMode::Shared,
                session_id: String::new(),
                human_confirm: HumanConfirm::None,
                repo_ref: None,
                workflow: flag(&args, "--workflow"),
            });
            println!("launched {sid}");
            drain_events(&events, None);
        }
        _ => {
            eprintln!(
                "usage: wicked-core <status | repos | register-repo --path <dir> | \
                 run --problem \"...\" [--repo <id>] [--confirm none|all|before:N] [--workflow <id>] | \
                 resume --session <id> | cancel --session <id> | \
                 launch --problem \"...\" [--workflow <id>]> [--db <path>]"
            );
            std::process::exit(2);
        }
    }
}

/// An interactive governed run: stream events and, at each human-confirm gate, prompt the operator
/// on stdin (a = approve, r = reject) and resolve the gate.
fn run_interactive(core: &Core, args: &[String]) {
    let Some(problem) = flag(args, "--problem") else {
        fail("run requires --problem \"...\"");
        return;
    };
    let repo_ref = flag(args, "--repo");
    let session_id = flag(args, "--session").unwrap_or_default();
    let events = core.subscribe();
    let run_id = match core.launch_run(LaunchSpec {
        problem,
        clis: registry_roster(),
        entity_mode: EntityMode::Shared,
        session_id,
        human_confirm: parse_confirm(args),
        repo_ref,
        workflow: flag(args, "--workflow"),
    }) {
        Ok(id) => id,
        Err(e) => {
            fail(&format!("run failed: {e}"));
            return;
        }
    };
    println!("running {run_id}");
    drain_events(&events, Some((core, &run_id)));
}

/// Print every event until the run reaches a terminal state. If `gate` is set, prompt the operator
/// at each `AwaitingHuman` and resolve it via `confirm_gate`.
fn drain_events(events: &std::sync::mpsc::Receiver<CoreEvent>, gate: Option<(&Core, &str)>) {
    loop {
        match events.recv_timeout(Duration::from_secs(3600)) {
            Ok(ev) => {
                println!("  {ev:?}");
                match &ev {
                    CoreEvent::AwaitingHuman { prompt, .. } => {
                        if let Some((core, run_id)) = gate {
                            let decision = prompt_decision(prompt);
                            match core.confirm_gate(run_id, decision) {
                                Ok(s) => println!("  → gate resolved: {s:?}"),
                                Err(e) => {
                                    fail(&format!("confirm_gate failed: {e}"));
                                    return;
                                }
                            }
                        }
                    }
                    CoreEvent::SessionCompleted { .. }
                    | CoreEvent::RunCancelled { .. }
                    | CoreEvent::SessionFailed { .. }
                    | CoreEvent::Error { .. } => break,
                    _ => {}
                }
            }
            Err(_) => {
                fail("timed out waiting for the run");
                return;
            }
        }
    }
}

/// Prompt the operator on stdin for a gate decision (a = approve, r = reject; default approve).
fn prompt_decision(prompt: &str) -> HumanDecision {
    println!("  ❓ {prompt}\n  [a]pprove / [r]eject ? ");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    match line.trim().chars().next() {
        Some('r') | Some('R') => HumanDecision::Reject,
        _ => HumanDecision::Approve { amend: None },
    }
}

fn print_status(core: &Core) {
    match core.sessions_detail() {
        Ok(views) if views.is_empty() => println!("(no sessions)"),
        Ok(views) => {
            for v in views {
                let done = v
                    .units
                    .iter()
                    .filter(|u| matches!(u.status, wicked_core::UnitStatus::Done))
                    .count();
                println!(
                    "{} [{:?}] {}/{} units done",
                    v.session.id,
                    v.session.status,
                    done,
                    v.units.len()
                );
            }
        }
        Err(e) => fail(&format!("status failed: {e}")),
    }
}

fn fail(msg: &str) {
    eprintln!("{msg}");
    std::process::exit(1);
}
