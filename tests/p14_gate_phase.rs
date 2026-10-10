//! Proves the `gate-phase` seam lets an operator arm a gate of their own on a step of a preset (X-MIG
//! M11: the built-ins are presets and the drop-in overlay is retired). `gate-phase` pins an APPROVED
//! validator onto a step whose catalog entry carries no pin and saves the result as a NEW preset,
//! leaving the base preset untouched. This test re-derives that produced artifact WITHOUT a live
//! `claude` call: it vaults an approved `DeterministicValidator` directly, pins it onto `feature`'s
//! `design` step, saves the preset through the same `put_preset` the command calls, resolves it back
//! and composes it — asserting the step carries the pin and the pin resolves to the approved validator.
//!
//! Plus arg-parse smokes: the flags are required, an unknown preset or step is named, a step of a
//! pinned entry is refused before anything is authored, and the usage string advertises it.

use std::process::Command;

use wicked_core::{
    builtin_presets, load_validator, pin, put_preset, resolve_preset, store_validator,
    DeterministicValidator, PresetSpec,
};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_wicked-core")
}

/// The heart of the gap-closure: a preset produced by pinning an approved validator onto a step (what
/// `gate-phase` saves) resolves with the pin ON the step, composes with the pin on that phase, and the
/// pin resolves to the approved validator in the vault — so the planner attaches it and the gate engages.
#[test]
fn gate_phase_preset_makes_a_step_actually_gate() {
    let dir = std::env::temp_dir().join(format!("wicked-gate-phase-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut store =
        wicked_apps_core::open_store(Some(dir.join("vault.db").to_str().unwrap())).unwrap();

    // 1. An APPROVED deterministic validator (what provision + approve would produce, minus `claude`).
    let validator = DeterministicValidator {
        criterion: "the design names a rollback plan".to_string(),
        script: "grep -q rollback DESIGN.md".to_string(),
        approved: true,
    };
    let approved_pin = store_validator(&mut store, &validator).expect("vault the validator");
    assert_eq!(
        approved_pin,
        pin(&validator),
        "store returns the content pin"
    );

    // 2. `feature`'s `design` step: its catalog entry carries no pin, so a step may set one.
    const STEP: &str = "design";
    let base = builtin_presets()
        .into_iter()
        .find(|(n, _)| *n == "feature")
        .expect("feature is a built-in preset")
        .1;
    assert!(
        base.iter().any(|s| s.id == STEP),
        "feature has a design step"
    );

    // 3. Pin it and save the gated preset under a fresh name (what `gate-phase` does).
    let name = format!("{STEP}-gated-feature");
    let steps: Vec<_> = base
        .iter()
        .cloned()
        .map(|mut s| {
            if s.id == STEP {
                s.validator_pin = Some(Some(approved_pin.clone()));
            }
            s
        })
        .collect();
    put_preset(
        &mut store,
        PresetSpec {
            name: name.clone(),
            project_id: None,
            steps,
            created_by: "gate-phase".to_string(),
        },
        1,
    )
    .expect("the gated preset saves (the step rules accept a pin on an unpinned entry)");

    // 4. It resolves by name, and its composed def carries the pin on that phase and nowhere new.
    let saved = resolve_preset(&store, None, &name)
        .expect("resolve")
        .expect("the gated preset resolves");
    let gated = saved.steps.iter().find(|s| s.id == STEP).unwrap();
    assert_eq!(gated.validator_pin, Some(Some(approved_pin.clone())));
    assert_eq!(
        saved.steps.iter().map(|s| &s.id).collect::<Vec<_>>(),
        base.iter().map(|s| &s.id).collect::<Vec<_>>(),
        "gate-phase reproduces the base step list exactly"
    );

    // 5. The pin resolves to the APPROVED validator — the read `attach_pinned_validators` performs.
    let resolved = load_validator(&store, &approved_pin)
        .expect("load must not error")
        .expect("the pinned validator is in the vault");
    assert!(resolved.approved);
    assert_eq!(resolved, validator);

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

/// `gate-phase` fails closed on an unknown preset name, naming the built-in presets — and never spawns
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
        "the error names the bad id and lists the built-in presets: {err}"
    );
    let _ = std::fs::remove_file(&db);
}

/// `gate-phase` fails closed on an unknown STEP id, naming the valid steps of the resolved preset.
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
        err.contains("no step `no-such-phase`")
            && err.contains("valid steps")
            && err.contains("build"),
        "the error names the bad step and lists the valid steps: {err}"
    );
    let _ = std::fs::remove_file(&db);
}

/// A step of a PINNED entry (`feature`'s `build`: the evidence floor) is refused before anything is
/// authored — a step may never swap its entry's floor.
#[test]
fn gate_phase_refuses_a_step_whose_entry_pins_its_own_floor() {
    let db = std::env::temp_dir().join(format!(
        "wicked-gate-phase-pinned-{}.db",
        std::process::id()
    ));
    let out = Command::new(bin())
        .args([
            "gate-phase",
            "--workflow",
            "feature",
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
        "a pinned entry's step must exit non-zero"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("pins its own floor"), "names why: {err}");
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

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
