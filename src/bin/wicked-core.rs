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
//!   wicked-core launch --problem "..."            # STUB self-test: deterministic stub output, no real CLI, no gates
//!   wicked-core gate-hook --scope S --phase P     # PreToolUse governance hook (claude invokes this)
//!   wicked-core output-gate-hook --scope S --phase P  # per-OUTPUT guardrail: governs generated
//!       # output text on stdin (policy-over-output + conformance-rule recall) → decisions.ndjson
//!   wicked-core provision-validator --criterion "..."   # author a deterministic validator (UNAPPROVED)
//!   wicked-core approve-validator --pin <pin>     # approve a vaulted validator → the pin to put in a def
//!   wicked-core gate-phase --workflow <base-id> --phase <phase-id> --criterion "..." [--out <dir>]
//!       # author+approve a validator for the criterion, PIN it onto that phase, and write a gated
//!       # drop-in workflow (new id) — the one path that turns a shipped, ungated workflow INTO a
//!       # gated one so the rev0.4 dual-validator gate actually engages
//!   wicked-core seed-domain-validators           # seed the deterministic coverage validator the
//!       # shipped domain-extraction.json gate pins, so that drop-in runs instead of failing closed
//!   wicked-core coverage [--out F]                # recompute front-half coverage FROM THE STORE →
//!       # coverage-report.json (schema-exact; two-predicate: bare/description-only behavior nodes are holes)
//!   wicked-core domain-graph [--coverage F] [--out F]  # translate the annotated estate graph into
//!       # requirements_graph.json (front-half coverage RECOMPUTED from the store, FAIL-CLOSED on < 1.0;
//!       # a supplied --coverage file is an optional cross-check that must agree; modern package-dir grouping)
//!   [--db <path>]                                 # else $WICKED_ESTATE_DB, else ./wicked-estate.db

use std::io::BufRead;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wicked_core::{
    registry_roster, run_gate_hook, run_output_gate_hook, Core, CoreEvent, EntityMode,
    HumanConfirm, HumanDecision, LaunchSpec, RepoSpec, WorkflowRegistry, WrappedCliStepRunner,
    ESTATE_DB_ENV, GATE_PHASE_ENV, GATE_SCOPE_ENV,
};

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// A non-empty environment variable, or `None`.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Resolve a gate-hook argument from `--flag` (standalone invocations) ELSE `env_var` (the launcher sets
/// it there so it never rides the shell-executed hook command). Empty if neither is set.
fn resolve_hook_arg(args: &[String], flag_name: &str, env_var: &str) -> String {
    flag(args, flag_name)
        .or_else(|| env_nonempty(env_var))
        .unwrap_or_default()
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
        // Resolve scope/phase/db from argv (standalone) ELSE the env the launcher sets. The injected
        // command carries NONE of these in the shell string (only the trusted exe) — scope/phase/db all
        // travel via env, so caller-controlled ids can't inject shell metacharacters (security fix).
        let scope = resolve_hook_arg(&args, "--scope", GATE_SCOPE_ENV);
        let phase = resolve_hook_arg(&args, "--phase", GATE_PHASE_ENV);
        let db = flag(&args, "--db").or_else(|| env_nonempty(ESTATE_DB_ENV));
        std::process::exit(run_gate_hook(&scope, &phase, db.as_deref()));
    }

    // output-gate-hook is the PER-OUTPUT sibling: same read-only-then-append discipline, but it
    // governs the generated OUTPUT text (on stdin) instead of a proposed tool input. Also exits with
    // the gate's code (2 = deny) and must run before `Core::spawn`.
    if args.get(1).map(String::as_str) == Some("output-gate-hook") {
        let scope = resolve_hook_arg(&args, "--scope", GATE_SCOPE_ENV);
        let phase = resolve_hook_arg(&args, "--phase", GATE_PHASE_ENV);
        let db = flag(&args, "--db").or_else(|| env_nonempty(ESTATE_DB_ENV));
        std::process::exit(run_output_gate_hook(&scope, &phase, db.as_deref()));
    }

    // provision-validator / approve-validator drive the rev0.4 pin+vault authoring flow DIRECTLY on the
    // store (author→approve→vault). Like gate-hook they must NOT spawn the actor — they open the store as
    // its SOLE writer for a brief command and exit — so handle them before `Core::spawn` (spawning the
    // actor too would put a second writer on the same SQLite file, breaking the single-writer invariant).
    match args.get(1).map(String::as_str) {
        Some("provision-validator") => return provision_validator_cmd(&args),
        Some("approve-validator") => return approve_validator_cmd(&args),
        Some("gate-phase") => return gate_phase_cmd(&args),
        Some("seed-domain-validators") => return seed_domain_validators_cmd(&args),
        Some("domain-graph") => return domain_graph_cmd(&args),
        Some("coverage") => return coverage_cmd(&args),
        _ => {}
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
            println!(
                "launched {sid} — STUB self-test path (deterministic stub output, no real CLI, no gates); \
                 use `run` for a real governed run"
            );
            drain_events(&events, None);
        }
        _ => {
            eprintln!(
                "usage: wicked-core <status | repos | register-repo --path <dir> | \
                 run --problem \"...\" [--repo <id>] [--confirm none|all|before:N] [--workflow <id>] | \
                 resume --session <id> | cancel --session <id> | \
                 launch --problem \"...\" [--workflow <id>] (STUB self-test — deterministic, no real CLI, no gates) | \
                 provision-validator --criterion \"...\" | approve-validator --pin <pin> | \
                 seed-domain-validators (seed the coverage validator for domain-extraction.json) | \
                 gate-phase --workflow <base-id> --phase <phase-id> --criterion \"...\" [--out <dir>] \
                 (author+approve+pin a validator onto a phase → a gated drop-in workflow)> [--db <path>]"
            );
            std::process::exit(2);
        }
    }
}

/// `provision-validator --criterion "..."`: author a deterministic validator for the criterion via the
/// live writer skill (a real `claude` call) and vault it UNAPPROVED, printing its pin. Opens the store
/// directly (sole writer; the actor is NOT spawned for this command) — see the note at the call site.
fn provision_validator_cmd(args: &[String]) {
    let Some(criterion) = flag(args, "--criterion") else {
        fail("provision-validator requires --criterion \"...\"");
        return;
    };
    let mut store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("provision-validator: open store failed: {e}"));
            return;
        }
    };
    let runner = WrappedCliStepRunner::default();
    match wicked_core::provision_validator(&criterion, &runner, &mut store) {
        Ok(pin) => {
            println!("provisioned UNAPPROVED validator, pin: {pin}");
            println!("approve it with:  wicked-core approve-validator --pin {pin}");
        }
        Err(e) => fail(&format!("provision-validator failed: {e}")),
    }
}

/// `approve-validator --pin <pin>`: approve a vaulted (unapproved) validator and print the APPROVED pin
/// the operator drops into a workflow def's `validator_pin`. Opens the store directly (sole writer).
fn approve_validator_cmd(args: &[String]) {
    let Some(pin) = flag(args, "--pin") else {
        fail("approve-validator requires --pin <pin>");
        return;
    };
    let mut store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("approve-validator: open store failed: {e}"));
            return;
        }
    };
    match wicked_core::approve_and_store(&mut store, &pin) {
        Ok(Some(approved)) => {
            println!("approved validator, pin: {approved}");
            println!("put this pin into a workflow def's `validator_pin`: {approved}");
        }
        Ok(None) => fail(&format!(
            "approve-validator: no vaulted validator with pin {pin}"
        )),
        Err(e) => fail(&format!("approve-validator failed: {e}")),
    }
}

/// The workflows overlay dir the planner resolves drop-ins from — `$WICKED_WORKFLOWS_DIR`, else
/// `$HOME/.config/wicked-core/workflows` (mirrors `pipeline::workflow_overlay_dir`). `gate-phase`
/// both READS this (to overlay operator drop-ins onto the built-ins before resolving `--workflow`)
/// and, absent `--out`, WRITES the gated def here so the very next `run --workflow <new-id>` sees it.
fn workflow_overlay_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("WICKED_WORKFLOWS_DIR") {
        return Some(std::path::PathBuf::from(d));
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/wicked-core/workflows"))
}

/// `seed-domain-validators`: seed the DETERMINISTIC, content-pinned coverage validator that the shipped
/// `workflows/domain-extraction.json` gate carries (`validator_pin`) into the vault, so the drop-in
/// actually runs instead of failing closed at plan time. Unlike `provision-validator` (a live LLM writer
/// whose script is nondeterministic and won't reproduce the pin), this vaults + approves the hand-authored
/// `coverage.py --check` port directly, yielding exactly `COVERAGE_VALIDATOR_PIN`. Idempotent
/// (content-addressed). Opens the store as its sole writer (actor not spawned), like the other vault
/// commands.
fn seed_domain_validators_cmd(args: &[String]) {
    let mut store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("seed-domain-validators: open store failed: {e}"));
            return;
        }
    };
    match wicked_core::provision_and_approve_coverage_validator(&mut store) {
        Ok(pin) => {
            println!("seeded + approved the domain-extraction coverage validator, pin: {pin}");
            println!(
                "(matches workflows/domain-extraction.json `validator_pin`; the drop-in now runs gated)"
            );
        }
        Err(e) => fail(&format!("seed-domain-validators failed: {e}")),
    }
}

/// `gate-phase --workflow <base-id> --phase <phase-id> --criterion "..." [--out <dir>]`: the one path
/// that turns a shipped-style, UNGATED workflow into a GATED one. The built-in feature/bug/migration
/// defs ship with `validator_pin: null` on every phase, so the rev0.4 dual-validator gate is INERT for
/// them — it only engages for a phase carrying a `validator_pin`. This command closes that: it loads the
/// base def, AUTHORS + APPROVES a deterministic validator for `--criterion` (a live `claude` call via the
/// writer skill, exactly like `provision-validator`), PINS the approved pin onto the named phase, and
/// writes the modified def as a NEW drop-in workflow JSON (fresh id, so it never clobbers the built-in)
/// into the workflows overlay dir. The operator then runs `run --workflow <new-id>` and the gate engages.
///
/// Opens the store directly as its SOLE writer (the actor is NOT spawned — same reason as
/// provision-validator/approve-validator). Fail-closed on an unknown workflow id or an unknown phase id
/// (both name the valid choices).
fn gate_phase_cmd(args: &[String]) {
    let Some(workflow) = flag(args, "--workflow") else {
        fail("gate-phase requires --workflow <base-id>");
        return;
    };
    let Some(phase) = flag(args, "--phase") else {
        fail("gate-phase requires --phase <phase-id>");
        return;
    };
    let Some(criterion) = flag(args, "--criterion") else {
        fail("gate-phase requires --criterion \"...\"");
        return;
    };

    // 1. Resolve the base WorkflowDef: the built-ins overlaid with operator drop-ins (the same seam the
    //    planner resolves against), so `--workflow` can name a shipped OR a previously dropped-in workflow.
    let mut reg = WorkflowRegistry::with_defaults();
    if let Some(dir) = workflow_overlay_dir() {
        if let Err(e) = reg.load_dir(&dir) {
            eprintln!(
                "gate-phase: workflow overlay {} failed to load ({e}); using built-ins only",
                dir.display()
            );
        }
    }
    let Some(base) = reg.get(&workflow) else {
        fail(&format!(
            "gate-phase: unknown workflow `{workflow}` — known workflows: {}",
            reg.ids().join(", ")
        ));
        return;
    };
    let mut def = base.clone();

    // 2. Fail-closed on an unknown phase id, NAMING the valid phases so the operator can correct it.
    if !def.phases.iter().any(|p| p.id == phase) {
        let valid: Vec<&str> = def.phases.iter().map(|p| p.id.as_str()).collect();
        fail(&format!(
            "gate-phase: workflow `{workflow}` has no phase `{phase}` — valid phases: {}",
            valid.join(", ")
        ));
        return;
    }

    // 3. AUTHOR + APPROVE a validator for the criterion (live `claude`), as the sole store writer.
    let mut store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("gate-phase: open store failed: {e}"));
            return;
        }
    };
    let runner = WrappedCliStepRunner::default();
    let unapproved = match wicked_core::provision_validator(&criterion, &runner, &mut store) {
        Ok(p) => p,
        Err(e) => {
            fail(&format!("gate-phase: authoring the validator failed: {e}"));
            return;
        }
    };
    let approved = match wicked_core::approve_and_store(&mut store, &unapproved) {
        Ok(Some(p)) => p,
        Ok(None) => {
            fail(&format!(
                "gate-phase: the just-authored validator (pin {unapproved}) was not found in the \
                 vault to approve"
            ));
            return;
        }
        Err(e) => {
            fail(&format!("gate-phase: approving the validator failed: {e}"));
            return;
        }
    };

    // 4. PIN the approved validator onto the phase and RE-ID the def so the drop-in never clobbers the
    //    built-in (a fresh id the operator selects with `run --workflow <new-id>`).
    let new_id = format!("{phase}-gated-{workflow}");
    def.id = new_id.clone();
    for p in def.phases.iter_mut() {
        if p.id == phase {
            p.validator_pin = Some(approved.clone());
        }
    }

    // 5. WRITE the gated def as a drop-in JSON: `--out` wins, else the resolved overlay dir (so the
    //    very next `run --workflow <new-id>` picks it up without any extra config).
    let Some(out_dir) = flag(args, "--out")
        .map(std::path::PathBuf::from)
        .or_else(workflow_overlay_dir)
    else {
        fail(
            "gate-phase: no output dir — pass --out <dir>, or set $WICKED_WORKFLOWS_DIR / $HOME so the \
             workflows overlay dir resolves",
        );
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        fail(&format!(
            "gate-phase: creating {} failed: {e}",
            out_dir.display()
        ));
        return;
    }
    let out_path = out_dir.join(format!("{new_id}.json"));
    let json = match serde_json::to_string_pretty(&def) {
        Ok(j) => j,
        Err(e) => {
            fail(&format!(
                "gate-phase: serializing the gated def failed: {e}"
            ));
            return;
        }
    };
    if let Err(e) = std::fs::write(&out_path, &json) {
        fail(&format!(
            "gate-phase: writing {} failed: {e}",
            out_path.display()
        ));
        return;
    }

    println!("gated workflow written: {}", out_path.display());
    println!("  new workflow id: {new_id}");
    println!("  phase `{phase}` now pins APPROVED validator: {approved}");
    println!(
        "the dual-validator gate now ENGAGES for phase `{phase}`. run it with:\n  \
         wicked-core run --problem \"...\" --workflow {new_id} --repo <id>"
    );
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

/// `wicked-core domain-graph` — translate the annotated estate graph into a `requirements_graph.json`
/// domain model (DES-OUTGOV-001 PR-D). Reads the front-half coverage report, gates on coverage == 1.0
/// (FAIL-CLOSED — refuses to translate an unannotated graph), builds the model (functional / package-
/// dir grouping, M5), and writes the artifact. Like the other pre-`Core::spawn` subcommands it opens
/// the store directly for a brief read and never spawns the actor.
///
/// STORE-BOUND coverage (core#25): front-half coverage is now RECOMPUTED directly from the store as the
/// PRIMARY source — the gate no longer trusts a separate file. A supplied `--coverage <file>` is an
/// optional cross-check that must AGREE with the recompute (fail-closed on disagreement). This closes the
/// trust-boundary hole (a stale report can no longer green-light a different graph) that the prior
/// increment left as a follow-on.
fn domain_graph_cmd(args: &[String]) {
    let out_path = flag(args, "--out")
        .unwrap_or_else(|| ".wicked-estate/requirements/requirements_graph.json".to_string());
    // The schema pins metadata.schema_version to const "1.0.0" — a consumer rejects a version it has
    // no validator for, so the emitted document must carry exactly this.
    let schema_version = flag(args, "--schema-version").unwrap_or_else(|| "1.0.0".to_string());

    let store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("domain-graph: open store failed: {e}"));
            return;
        }
    };

    // Front-half coverage is RECOMPUTED from the store (PRIMARY — the store is the source of truth, not a
    // trusted external file). A supplied `--coverage <file>` is an optional CROSS-CHECK that must AGREE;
    // an absent/unsupplied file is NOT an error (recompute stands). (DES-OUTGOV-005 decision #4.)
    let coverage = match wicked_governance::recompute_front_half_coverage(&store) {
        Ok(c) => c,
        Err(e) => {
            fail(&format!("domain-graph: coverage recompute failed: {e}"));
            return;
        }
    };
    if let Some(coverage_path) = flag(args, "--coverage") {
        match std::fs::read_to_string(&coverage_path) {
            Ok(s) => match serde_json::from_str::<wicked_governance::CoverageReport>(&s) {
                Ok(file) => {
                    // "Must agree" = every EXACT integer count matches the store recompute (unaccounted is
                    // the gate field, but a mismatch in any count means a different/stale graph); `coverage`
                    // is a rounded ratio, so compare it only with a generous tolerance (not f64::EPSILON,
                    // which spuriously fails on JSON-parse/round drift).
                    let ints_disagree = file.behavior_bearing != coverage.behavior_bearing
                        || file.resolved != coverage.resolved
                        || file.risk_flagged != coverage.risk_flagged
                        || file.unaccounted != coverage.unaccounted;
                    if ints_disagree || (file.coverage - coverage.coverage).abs() > 1e-4 {
                        fail(&format!(
                            "domain-graph: supplied --coverage {coverage_path} DISAGREES with the store \
                             recompute (file coverage={:.4}/unaccounted={}, store coverage={:.4}/unaccounted={}) \
                             — refusing (fail-closed)",
                            file.coverage, file.unaccounted, coverage.coverage, coverage.unaccounted
                        ));
                        return;
                    }
                }
                Err(e) => {
                    fail(&format!(
                        "domain-graph: supplied --coverage {coverage_path} is not valid JSON: {e}"
                    ));
                    return;
                }
            },
            Err(e) => {
                fail(&format!(
                    "domain-graph: supplied --coverage {coverage_path} cannot be read: {e}"
                ));
                return;
            }
        }
    }

    // Fail-closed: `build_domain_model` bails when coverage < 1.0 (never translates a partial graph) AND
    // recomputes internally, so a store hole denies even if the passed report claimed 1.0.
    let model = match wicked_governance::build_domain_model(&store, &coverage, &schema_version) {
        Ok(m) => m,
        Err(e) => {
            fail(&format!("domain-graph: {e}"));
            return;
        }
    };

    if let Some(parent) = std::path::Path::new(&out_path).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            fail(&format!(
                "domain-graph: cannot create {}: {e}",
                parent.display()
            ));
            return;
        }
    }
    let json = serde_json::to_string_pretty(&model).expect("DomainModel serializes to JSON");
    match std::fs::write(&out_path, json) {
        Ok(()) => println!(
            "domain-graph: wrote {} domain(s) → {out_path}",
            model.domains.len()
        ),
        Err(e) => fail(&format!("domain-graph: cannot write {out_path}: {e}")),
    }
}

/// `wicked-core coverage [--out F]` — recompute the front-half coverage report DIRECTLY from the store
/// and emit `coverage-report.json` (schema-exact). `--out` defaults to a bare `coverage-report.json` in
/// the cwd so the shipped grep validator (which reads that path from the phase worktree) finds it
/// (DES-OUTGOV-005 decision #4). Opens the store directly for a brief read; never spawns the actor.
fn coverage_cmd(args: &[String]) {
    let out_path = flag(args, "--out").unwrap_or_else(|| "coverage-report.json".to_string());
    let store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("coverage: open store failed: {e}"));
            return;
        }
    };
    let report = match wicked_governance::recompute_front_half_coverage(&store) {
        Ok(r) => r,
        Err(e) => {
            fail(&format!("coverage: recompute failed: {e}"));
            return;
        }
    };
    if let Some(parent) = std::path::Path::new(&out_path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                fail(&format!(
                    "coverage: cannot create {}: {e}",
                    parent.display()
                ));
                return;
            }
        }
    }
    let json = serde_json::to_string_pretty(&report).expect("CoverageReport serializes to JSON");
    match std::fs::write(&out_path, json) {
        Ok(()) => println!(
            "coverage: {:.4} ({} behavior-bearing, {} unaccounted) → {out_path}",
            report.coverage, report.behavior_bearing, report.unaccounted
        ),
        Err(e) => fail(&format!("coverage: cannot write {out_path}: {e}")),
    }
}

fn fail(msg: &str) {
    eprintln!("{msg}");
    std::process::exit(1);
}
