//! DOMAIN-EXTRACTION — what the `domain-extraction` built-in preset needs beyond its data (J6 v3).
//!
//! The workflow is DATA: the built-in preset (`crate::catalog`), two host steps, `domain-run` (garden's
//! `scripts/domain/run.py` on the run's repository, with the person's scope) and `domain-graph`. This
//! module owns the scope a launch carries (its keys, its size, a strict parse) and the argv of a garden
//! domain step. (J6 C3) The coverage validator, its pin, its seed command and the minimum it checked
//! went with the old page loop: a domain model run has no share to reach.

/// The id of the built-in preset this module gates (`domain-extraction`).
pub const DOMAIN_EXTRACTION_WORKFLOW_ID: &str = "domain-extraction";

/// The keys a domain-extraction scope may carry (garden `scripts/domain/run.py` `SCOPE_KEYS`, which
/// re-checks the same shape when the step reads it).
pub const SCOPE_KEYS: [&str; 4] = ["intent", "phrases", "scope_hash", "description"];

/// The largest scope a launch may carry, as compact JSON (garden's `SCOPE_MAX_BYTES`).
pub const SCOPE_MAX_BYTES: usize = 64 * 1024;

/// (J6 C1b) The launch's check of [`crate::LaunchSpec::scope`] against the workflow it names: a
/// `domain-extraction` launch needs a scope, and no other launch takes one. A scope is a JSON object
/// of [`SCOPE_KEYS`] only, with `intent` a non-empty string, `phrases` (when present, `null`
/// included) a list of non-empty strings, `scope_hash` and `description` strings, and at most
/// [`SCOPE_MAX_BYTES`]. Every refusal names what is wrong.
pub fn check_launch_scope(
    workflow: Option<&str>,
    scope: Option<&serde_json::Value>,
) -> Result<(), String> {
    let domain = workflow == Some(DOMAIN_EXTRACTION_WORKFLOW_ID);
    let Some(scope) = scope else {
        return if domain {
            Err("domain-extraction needs a scope (Start carries it from the preview)".to_string())
        } else {
            Ok(())
        };
    };
    if !domain {
        return Err(format!(
            "a scope is only for a {DOMAIN_EXTRACTION_WORKFLOW_ID} launch; this launch runs {}",
            workflow.unwrap_or("no preset")
        ));
    }
    let size = serde_json::to_vec(scope).map_or(usize::MAX, |b| b.len());
    if size > SCOPE_MAX_BYTES {
        return Err(format!("the scope is over {SCOPE_MAX_BYTES} bytes"));
    }
    let Some(obj) = scope.as_object() else {
        return Err("the scope is not a JSON object".to_string());
    };
    let extra: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !SCOPE_KEYS.contains(k))
        .collect();
    if !extra.is_empty() {
        return Err(format!("the scope has unknown keys: {}", extra.join(", ")));
    }
    let filled = |v: &serde_json::Value| v.as_str().is_some_and(|s| !s.trim().is_empty());
    if !obj.get("intent").is_some_and(filled) {
        return Err("the scope has no intent (a non-empty string)".to_string());
    }
    if let Some(ph) = obj.get("phrases") {
        if !ph.as_array().is_some_and(|a| a.iter().all(filled)) {
            return Err("the scope's phrases are not a list of non-empty strings".to_string());
        }
    }
    for k in ["scope_hash", "description"] {
        if obj.get(k).is_some_and(|v| !v.is_string()) {
            return Err(format!("the scope's {k} is not a string"));
        }
    }
    Ok(())
}

/// (J6 C1b) Parse a launcher's scope JSON (core-ts `scopeJson`) without losing a field: an object
/// key that appears twice is refused, as garden's own reader refuses it, where a plain parse would
/// keep the last one and drop the first silently. Lone surrogate escapes and non-JSON text are
/// refused by the parser itself. The shape is then [`check_launch_scope`]'s.
pub fn parse_scope_json(text: &str) -> Result<serde_json::Value, String> {
    use serde::de::{Error as _, MapAccess, SeqAccess, Visitor};
    use serde_json::Value;
    struct Strict(Value);
    struct Strictly;
    impl<'de> serde::Deserialize<'de> for Strict {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Strictly).map(Strict)
        }
    }
    impl<'de> Visitor<'de> for Strictly {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON value")
        }
        fn visit_unit<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
            Ok(Value::Bool(v))
        }
        fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
            Ok(Value::from(v))
        }
        fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
            Ok(Value::from(v))
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
            serde_json::Number::from_f64(v)
                .map(Value::Number)
                .ok_or_else(|| E::custom("a number that is not finite"))
        }
        fn visit_str<E>(self, v: &str) -> Result<Value, E> {
            Ok(Value::String(v.to_owned()))
        }
        fn visit_string<E>(self, v: String) -> Result<Value, E> {
            Ok(Value::String(v))
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
            let mut out = Vec::new();
            while let Some(Strict(v)) = a.next_element()? {
                out.push(v);
            }
            Ok(Value::Array(out))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
            let mut out = serde_json::Map::new();
            while let Some(k) = a.next_key::<String>()? {
                if out.contains_key(&k) {
                    return Err(A::Error::custom(format!("a key appears twice: {k}")));
                }
                let Strict(v) = a.next_value()?;
                out.insert(k, v);
            }
            Ok(Value::Object(out))
        }
    }
    serde_json::from_str::<Strict>(text)
        .map(|s| s.0)
        .map_err(|e| format!("the scope is not JSON: {e}"))
}

/// The argv of a host step that runs a garden domain script on the run's repository: through the
/// garden generation the run was admitted against (`WICKED_GARDEN_ROOT`, handed to every generic
/// Tool unit — never a `wicked-garden` from the daemon's PATH), with the store and the repository
/// root as positional parameters so the planner binds the `{code_graph_db}` / `{repo_root}`
/// placeholders (whole arguments only, [`crate::plan::bind_repo_paths`]).
pub fn garden_domain_step_cmd(step: &str, script: &str, rest: &str) -> Vec<String> {
    vec![
        "bash".to_string(),
        "-c".to_string(),
        format!(
            "exec \"${{WICKED_GARDEN_ROOT:?{NO_GARDEN_ROOT}}}/scripts/wicked-garden\" run {script} --db \"$1\" --repo-root \"$2\" {rest}"
        ),
        step.to_string(),
        crate::workflow::CODE_GRAPH_DB_TOKEN.to_string(),
        crate::workflow::REPO_ROOT_TOKEN.to_string(),
    ]
}

/// The refusal of a garden step handed no garden root (never a `wicked-garden` from PATH).
const NO_GARDEN_ROOT: &str =
    "the engine handed this Tool phase no garden root; refusing to resolve wicked-garden from PATH";

/// garden's J6 v3 domain run, the script `domain-run` runs.
pub const DOMAIN_RUN_SCRIPT: &str = "scripts/domain/run.py";

/// (core#1955 R2-6) What `domain-run` says when the admitted garden predates J6 v3 (it has no
/// [`DOMAIN_RUN_SCRIPT`]), instead of the launcher's missing-script error.
pub const OLD_GARDEN_REFUSAL: &str = "this wicked-garden is too old to map a domain: upgrade it";

/// The `domain-run` step: [`garden_domain_step_cmd`] for [`DOMAIN_RUN_SCRIPT`], refusing in words
/// ([`OLD_GARDEN_REFUSAL`], exit 1) before it runs anything when the admitted garden root has no
/// such script. The check runs in the step because the garden root is resolved per Tool unit (the
/// generation the run was admitted against), not at launch.
pub fn domain_run_step_cmd(step: &str, rest: &str) -> Vec<String> {
    // RED (core#1955): no check, the old garden's launcher answers.
    let _ = OLD_GARDEN_REFUSAL;
    garden_domain_step_cmd(step, DOMAIN_RUN_SCRIPT, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::GateSpec;
    use crate::{domain::StageKind, plan::plan_from_def};

    /// The shipped def: the built-in preset as the engine composes it (X-MIG M6/M11).
    fn load_shipped_def() -> crate::workflow::WorkflowDef {
        crate::preset::builtin_preset_def(DOMAIN_EXTRACTION_WORKFLOW_ID)
    }

    /// (J6 v3) The shipped preset is two host steps, `domain-run` then `domain-graph`, with no
    /// seat unit, no skill, no gate type, no human gate and no validator pin: a run launched with
    /// no human confirm ends with no gate. Fails on the old 9-step preset.
    #[test]
    fn the_shipped_preset_is_two_host_steps_with_no_gate() {
        let def = load_shipped_def();
        assert_eq!(def.id, DOMAIN_EXTRACTION_WORKFLOW_ID);
        let ids: Vec<&str> = def.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, [crate::catalog::DOMAIN_RUN_STEP, "domain-graph"]);
        def.validate().expect("shipped def is a valid WorkflowDef");
        for p in &def.phases {
            assert!(
                matches!(p.executor, crate::workflow::PhaseExecutor::Tool { .. }),
                "{} is a host step",
                p.id
            );
            assert!(p.skill_ref.is_none(), "{} runs no skill", p.id);
            assert_eq!(p.gate_type, None, "{} has no gate type", p.id);
            assert!(
                !matches!(
                    p.gate,
                    GateSpec::HumanConfirm { .. } | GateSpec::HumanConfirmIf(_)
                ),
                "{} has no human gate: {:?}",
                p.id,
                p.gate
            );
            assert!(p.validator_pin.is_none(), "{} carries no pin", p.id);
        }
        let dg = &def.phases[1];
        assert_eq!(dg.depends_on, [crate::catalog::DOMAIN_RUN_STEP]);
        assert_eq!(dg.kind, StageKind::Build);
        let units = plan_from_def(&def, "map the payments service", "s1");
        assert_eq!(units.len(), 2);
        assert!(units
            .iter()
            .all(|u| u.tool_cmd.is_some() && u.skill_ref.is_none()));
    }

    /// (J6 C1b) `domain-run` runs garden's `run.py` from the admitted garden root and reads the
    /// scope file core wrote into the step's channel directory, refusing by name when there is
    /// none. `domain-graph` builds into the repository's store (`--db`) for the checkout
    /// (`--repo-root`) and writes no file into the run.
    #[test]
    fn domain_run_reads_the_scope_core_hands_it_and_domain_graph_builds_into_the_store() {
        let def = load_shipped_def();
        let cmd = |i: usize| match &def.phases[i].executor {
            crate::workflow::PhaseExecutor::Tool { cmd } => cmd.clone(),
            other => panic!("not a Tool: {other:?}"),
        };
        let run = cmd(0);
        assert_eq!(run[..2], ["bash", "-c"]);
        assert!(
            run[2].contains("run scripts/domain/run.py --db \"$1\" --repo-root \"$2\" --scope \"${WICKED_DOMAIN_SCOPE:?"),
            "{run:?}"
        );
        assert!(run[2].contains("${WICKED_GARDEN_ROOT:?"), "{run:?}");
        assert_eq!(
            run[3..],
            [
                crate::catalog::DOMAIN_RUN_STEP,
                crate::workflow::CODE_GRAPH_DB_TOKEN,
                crate::workflow::REPO_ROOT_TOKEN
            ]
        );
        assert_eq!(crate::model_call::SCOPE_ENV, "WICKED_DOMAIN_SCOPE");
        let graph = cmd(1);
        assert_eq!(
            graph,
            [
                "wicked-core",
                "domain-graph",
                "--db",
                crate::workflow::CODE_GRAPH_DB_TOKEN,
                "--repo-root",
                crate::workflow::REPO_ROOT_TOKEN
            ]
        );
    }

    /// (core#1955 R2-6) `domain-run` refuses in words, before it runs anything, when the admitted
    /// garden has no `scripts/domain/run.py`. A garden with it runs it through its launcher, and no
    /// garden root keeps the PATH refusal. Fails without the check: the old garden's launcher runs.
    #[cfg(unix)]
    #[test]
    fn domain_run_refuses_an_old_garden_in_words() {
        use std::os::unix::fs::PermissionsExt;
        let cmd = domain_run_step_cmd("domain-run", "--scope \"$WICKED_DOMAIN_SCOPE\"");
        let dir =
            std::env::temp_dir().join(format!("wicked-core-old-garden-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let launcher = dir.join("scripts").join("wicked-garden");
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        std::fs::write(&launcher, "#!/bin/sh\necho \"launched $*\"\nexit 3\n").unwrap();
        std::fs::set_permissions(launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |root: Option<&std::path::Path>| {
            let mut c = std::process::Command::new(&cmd[0]);
            c.arg(&cmd[1])
                .arg(&cmd[2])
                .args(["domain-run", "/db", "/repo"])
                .env("WICKED_DOMAIN_SCOPE", "/scope.json")
                .env_remove("WICKED_GARDEN_ROOT");
            if let Some(r) = root {
                c.env("WICKED_GARDEN_ROOT", r);
            }
            c.output().unwrap()
        };
        // An old garden: its launcher is there, the J6 v3 script is not.
        let old = run(Some(&dir));
        assert_eq!(old.status.code(), Some(1), "{old:?}");
        assert_eq!(
            String::from_utf8_lossy(&old.stderr).trim(),
            OLD_GARDEN_REFUSAL
        );
        assert!(old.stdout.is_empty(), "nothing ran: {old:?}");
        // A garden with the script runs it through its own launcher.
        std::fs::create_dir_all(dir.join("scripts").join("domain")).unwrap();
        std::fs::write(dir.join(DOMAIN_RUN_SCRIPT), "").unwrap();
        let new = run(Some(&dir));
        assert_eq!(new.status.code(), Some(3), "{new:?}");
        assert_eq!(
            String::from_utf8_lossy(&new.stdout).trim(),
            "launched run scripts/domain/run.py --db /db --repo-root /repo --scope /scope.json"
        );
        // No garden root: the PATH refusal, not the old-garden one.
        let none = run(None);
        assert_ne!(none.status.code(), Some(0), "{none:?}");
        let err = String::from_utf8_lossy(&none.stderr);
        assert!(
            err.contains(NO_GARDEN_ROOT) && !err.contains(OLD_GARDEN_REFUSAL),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// (J6 C1b, codex r1) A launcher's scope text parses without losing a field: a key twice, at
    /// the top or nested, is refused by name, as garden refuses it; a lone surrogate and non-JSON
    /// are refused; a well-formed scope parses to the same value a plain parse gives.
    #[test]
    fn a_scope_with_a_key_twice_or_bad_text_is_refused_at_parse() {
        for (text, why) in [
            (
                r#"{"intent":"a","scope_hash":"h","scope_hash":""}"#,
                "a key appears twice: scope_hash",
            ),
            (
                r#"{"intent":"a","intent":"b"}"#,
                "a key appears twice: intent",
            ),
            (
                r#"{"intent":"a","phrases":["x"],"phrases":[]}"#,
                "a key appears twice: phrases",
            ),
            (
                r#"{"intent":"a","x":{"k":1,"k":2}}"#,
                "a key appears twice: k",
            ),
            (r#"{"intent":"\ud800"}"#, "the scope is not JSON"),
            ("not json", "the scope is not JSON"),
        ] {
            let err = parse_scope_json(text).unwrap_err();
            assert!(err.contains(why), "{text}: {err}");
        }
        let text = r#"{"intent":"refunds","phrases":["remboursé"],"n":[1,-2,2.5,null,true]}"#;
        assert_eq!(
            parse_scope_json(text).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }

    /// (J6 C1b, coord ruling B') The launch's scope check, refusal by refusal. Each case fails
    /// without its clause.
    #[test]
    fn a_launch_scope_is_checked_and_each_refusal_names_why() {
        use serde_json::json;
        let de = Some(DOMAIN_EXTRACTION_WORKFLOW_ID);
        let ok = json!({"intent": "payments", "phrases": ["refund"], "scope_hash": "ab", "description": "d"});
        assert_eq!(check_launch_scope(de, Some(&ok)), Ok(()));
        assert_eq!(
            check_launch_scope(de, Some(&json!({"intent": "x"}))),
            Ok(())
        );
        // No scope: refused on domain-extraction, fine elsewhere.
        assert_eq!(
            check_launch_scope(de, None).unwrap_err(),
            "domain-extraction needs a scope (Start carries it from the preview)"
        );
        assert_eq!(check_launch_scope(Some("feature"), None), Ok(()));
        assert_eq!(check_launch_scope(None, None), Ok(()));
        // A scope on another launch is refused, never dropped.
        let other = check_launch_scope(Some("feature"), Some(&ok)).unwrap_err();
        assert!(
            other.contains("only for a domain-extraction launch") && other.contains("feature"),
            "{other}"
        );
        assert!(check_launch_scope(None, Some(&ok))
            .unwrap_err()
            .contains("no preset"));
        for (bad, why) in [
            (json!("payments"), "the scope is not a JSON object"),
            (json!(["payments"]), "the scope is not a JSON object"),
            (
                json!({"intent": "x", "extra": 1, "more": 2}),
                "the scope has unknown keys: extra, more",
            ),
            (json!({}), "the scope has no intent (a non-empty string)"),
            (
                json!({"intent": "  "}),
                "the scope has no intent (a non-empty string)",
            ),
            (
                json!({"intent": 3}),
                "the scope has no intent (a non-empty string)",
            ),
            (
                json!({"intent": null}),
                "the scope has no intent (a non-empty string)",
            ),
            (
                json!({"intent": "x", "phrases": null}),
                "the scope's phrases are not a list of non-empty strings",
            ),
            (
                json!({"intent": "x", "phrases": "refund"}),
                "the scope's phrases are not a list of non-empty strings",
            ),
            (
                json!({"intent": "x", "phrases": ["refund", " "]}),
                "the scope's phrases are not a list of non-empty strings",
            ),
            (
                json!({"intent": "x", "phrases": [1]}),
                "the scope's phrases are not a list of non-empty strings",
            ),
            (
                json!({"intent": "x", "scope_hash": 1}),
                "the scope's scope_hash is not a string",
            ),
            (
                json!({"intent": "x", "description": null}),
                "the scope's description is not a string",
            ),
        ] {
            assert_eq!(
                check_launch_scope(de, Some(&bad)).unwrap_err(),
                why,
                "{bad}"
            );
        }
        // The size bound is on the compact JSON: one byte over refuses, the bound itself passes.
        let pad = |n: usize| json!({"intent": "x", "description": "d".repeat(n)});
        let base = serde_json::to_vec(&pad(0)).unwrap().len();
        assert_eq!(
            check_launch_scope(de, Some(&pad(SCOPE_MAX_BYTES - base))),
            Ok(())
        );
        assert_eq!(
            check_launch_scope(de, Some(&pad(SCOPE_MAX_BYTES - base + 1))).unwrap_err(),
            "the scope is over 65536 bytes"
        );
    }
}
