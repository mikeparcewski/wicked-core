//! Proves the `gate-phase` seam closes the "shipped workflows are ungated" gap: a produced drop-in
//! workflow genuinely ENGAGES the rev0.4 dual-validator gate. The built-in feature/bug/migration defs
//! ship with `validator_pin: null` on every phase, so the gate is INERT for them; `gate-phase` pins an
//! APPROVED validator onto a phase and writes a re-id'd drop-in. This test re-derives that produced
//! artifact WITHOUT a live `claude` call: it builds an approved `DeterministicValidator` directly,
//! vaults it, pins it onto a `feature_def` phase, serializes to a temp overlay dir, and then loads the
//! drop-in back through the SAME registry the planner uses — asserting the phase carries the pin and the
//! pin resolves to the approved validator (i.e. `attach_pinned_validators` would attach it and gate).
//!
//! Plus an arg-parse smoke that the `gate-phase` subcommand validates its flags and is advertised in
//! the usage string (mirrors the existing `cli_smoke` provision/approve checks) — no live CLI.

use std::process::Command;

use wicked_core::{
    feature_def, load_validator, pin, store_validator, DeterministicValidator, WorkflowRegistry,
};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_wicked-core")
}

/// The heart of the gap-closure: a drop-in produced by pinning an approved validator onto a phase
/// (exactly what `gate-phase` writes) loads back with the pin ON the phase, and that pin resolves to
/// the approved validator in the vault — so the planner's `attach_pinned_validators` would attach it
/// and the dual-validator gate ENGAGES. Done deterministically (no `claude`): store the validator +
/// serialize the def ourselves, then reload through `WorkflowRegistry::with_defaults().load_dir(dir)`.
#[test]
fn gate_phase_drop_in_makes_a_shipped_style_workflow_actually_gate() {
    let dir = std::env::temp_dir().join(format!("wicked-gate-phase-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A vault store (the sole writer, like the `gate-phase` command opens it).
    let mut store =
        wicked_apps_core::open_store(Some(dir.join("vault.db").to_str().unwrap())).unwrap();

    // 1. An APPROVED deterministic validator — the artifact `provision_validator` + `approve_and_store`
    //    would produce, but built directly so this test never calls `claude`.
    let validator = DeterministicValidator {
        criterion: "the build produced a non-empty CHANGELOG entry".to_string(),
        script: "test -s CHANGELOG.md".to_string(),
        approved: true,
    };
    let approved_pin = store_validator(&mut store, &validator).expect("vault the validator");
    assert_eq!(
        approved_pin,
        pin(&validator),
        "store returns the content pin"
    );

    // 2. The base shipped def ships UNGATED — its `build` phase carries no validator_pin. Prove that,
    //    so the pinning below is a genuine change of state (inert → engaged).
    let base = feature_def();
    const PHASE: &str = "build";
    let base_build = base
        .phases
        .iter()
        .find(|p| p.id == PHASE)
        .expect("feature def has a build phase");
    assert!(
        base_build.validator_pin.is_none(),
        "the shipped feature `build` phase is ungated (validator_pin: null) — the gap this closes"
    );

    // 3. Pin the approved validator onto that phase and RE-ID the def (what `gate-phase` does), then
    //    serialize the drop-in to the overlay dir as pretty JSON.
    let new_id = format!("{PHASE}-gated-{}", base.id);
    let mut gated = base.clone();
    gated.id = new_id.clone();
    for p in gated.phases.iter_mut() {
        if p.id == PHASE {
            p.validator_pin = Some(approved_pin.clone());
        }
    }
    let overlay = dir.join("workflows");
    std::fs::create_dir_all(&overlay).unwrap();
    std::fs::write(
        overlay.join(format!("{new_id}.json")),
        serde_json::to_string_pretty(&gated).unwrap(),
    )
    .unwrap();

    // 4. Reload through the SAME registry seam the planner uses: built-ins + the operator overlay dir.
    let mut reg = WorkflowRegistry::with_defaults();
    let loaded_ids = reg.load_dir(&overlay).expect("load_dir the overlay");
    assert!(
        loaded_ids.contains(&new_id),
        "the drop-in registered under its new id: {loaded_ids:?}"
    );

    // The new id resolves and did NOT clobber the built-in (both are present).
    let resolved = reg.get(&new_id).expect("the gated drop-in resolves");
    assert!(
        reg.get(&base.id).is_some(),
        "the built-in `{}` is untouched — the drop-in used a fresh id",
        base.id
    );

    // 5a. The reloaded def carries the pin ON the target phase (and nowhere else).
    let reloaded_build = resolved
        .phases
        .iter()
        .find(|p| p.id == PHASE)
        .expect("gated def has the build phase");
    assert_eq!(
        reloaded_build.validator_pin.as_deref(),
        Some(approved_pin.as_str()),
        "the build phase carries the approved pin — the gate is armed"
    );
    assert!(
        resolved
            .phases
            .iter()
            .filter(|p| p.id != PHASE)
            .all(|p| p.validator_pin.is_none()),
        "only the named phase was gated"
    );

    // 5b. That pin resolves to the APPROVED validator in the vault — the exact read
    //     `attach_pinned_validators` performs to attach it and engage the gate. This is the proof the
    //     produced drop-in genuinely gates, not just that a string was copied.
    let resolved_validator =
        load_validator(&store, reloaded_build.validator_pin.as_deref().unwrap())
            .expect("load must not error")
            .expect("the pinned validator is in the vault");
    assert!(
        resolved_validator.approved,
        "the pin resolves to an APPROVED validator — the planner attaches it (fail-closed on unapproved)"
    );
    assert_eq!(
        resolved_validator, validator,
        "the pinned validator is exactly the one we approved"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Arg-parse smoke: `gate-phase` with no flags fails BEFORE any store/`claude` call, naming the first
/// missing flag. Mirrors the `provision-validator`/`approve-validator` smoke tests.
#[test]
fn gate_phase_requires_its_flags() {
    let out = Command::new(bin())
        .arg("gate-phase")
        .output()
        .expect("run wicked-core");
    assert!(
        !out.status.success(),
        "gate-phase with no flags must exit non-zero"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--workflow"),
        "the error names the first missing flag: {err}"
    );
}

/// `gate-phase` fails closed on an unknown workflow id, naming the known workflows — and never spawns
/// the actor or calls `claude` (the check happens before any store write).
#[test]
fn gate_phase_rejects_an_unknown_workflow() {
    let db = std::env::temp_dir().join(format!(
        "wicked-gate-phase-unknown-{}.db",
        std::process::id()
    ));
    let out = Command::new(bin())
        .args([
            "gate-phase",
            "--workflow",
            "no-such-workflow-xyz",
            "--phase",
            "build",
            "--criterion",
            "anything",
            "--db",
        ])
        .arg(&db)
        .output()
        .expect("run wicked-core");
    assert!(
        !out.status.success(),
        "an unknown workflow must exit non-zero"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("unknown workflow") && err.contains("feature"),
        "the error names the bad id and lists the known workflows: {err}"
    );
    let _ = std::fs::remove_file(&db);
}

/// `gate-phase` fails closed on an unknown PHASE id, naming the valid phases of the resolved workflow.
#[test]
fn gate_phase_rejects_an_unknown_phase_naming_the_valid_ones() {
    let db = std::env::temp_dir().join(format!(
        "wicked-gate-phase-badphase-{}.db",
        std::process::id()
    ));
    let out = Command::new(bin())
        .args([
            "gate-phase",
            "--workflow",
            "feature",
            "--phase",
            "no-such-phase",
            "--criterion",
            "anything",
            "--db",
        ])
        .arg(&db)
        .output()
        .expect("run wicked-core");
    assert!(!out.status.success(), "an unknown phase must exit non-zero");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no phase `no-such-phase`")
            && err.contains("valid phases")
            && err.contains("build"),
        "the error names the bad phase and lists the valid phases: {err}"
    );
    let _ = std::fs::remove_file(&db);
}

/// The usage string advertises the `gate-phase` subcommand (an unknown subcommand prints usage).
#[test]
fn usage_advertises_gate_phase() {
    let db =
        std::env::temp_dir().join(format!("wicked-gate-phase-usage-{}.db", std::process::id()));
    let out = Command::new(bin())
        .args(["bogus-subcommand", "--db"])
        .arg(&db)
        .output()
        .expect("run wicked-core");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("gate-phase"),
        "usage advertises the gate-phase subcommand: {err}"
    );
    let _ = std::fs::remove_file(&db);
}
