//! The phase catalog (DES-TEAMING-002 §8.3): the one place every phase TYPE a plan may use is
//! defined. Data and nothing else — no workflow file, no per-surface copy.
//!
//! Each entry IS a [`PhaseDef`], so every existing control keeps working through the mechanism
//! that already enforces it: `plan_from_def` copies `gate` / `role` / `owner` onto the unit,
//! `attach_pinned_validators` attaches each `validator_pin`, and the fence reads `role`. A plan's
//! steps are composed onto these entries by [`crate::plan::compose`], which lets a step only make
//! its entry STRICTER (raise the gate, add a pin to an unpinned entry, set `executes_code`) and
//! never touch `role` or a pin the entry carries.
//!
//! The evidence floor lives HERE (DES-TEAMING-002 §10, "moved in seam C1"): the `build`, `test`,
//! `review` and `security_review` entries carry [`EVIDENCE_FLOOR_PIN`] as data, so a composed
//! plan's code-writing and code-judging steps are floored by the catalog, not by each def.
//!
//! `domain_coverage` carries [`COVERAGE_VALIDATOR_PIN`] as data (the shipped domain-extraction
//! `coverage` phase's pin). A step may not swap a pin its entry carries, so the coverage judge is
//! its own entry rather than a pin swap on `test`.
//!
//! Deviations from the §8.3 table, each forced by "compose of today's def equals today's def"
//! (seam C1 acceptance (a)) and recorded in the C1 PR:
//! - `run` has no single kind in the table ("per step"); the entry's default is `recon`, the kind
//!   today's tool phases mostly carry, and a `run` step may set its own.
//! - `deliver` is `executes_code: false` (the table says `true`): crew's composed deliver phase
//!   (`deliverPrPhase`, crew `packages/crew/src/core/deliver.ts:798`) is an engine-owned Tool
//!   phase with `executes_code: false`, and a step may never lower `executes_code`.
//! - `Tool` entries (`run`, `deliver`) carry an EMPTY command: the step supplies it, and
//!   `compose` refuses a Tool step that does not.

use std::sync::OnceLock;

use crate::builtin_floors::{EVIDENCE_FLOOR_PIN, WALKTHROUGH_LINT_PIN, WALKTHROUGH_RESULT_PIN};
use crate::domain::StageKind;
use crate::domain_extraction::COVERAGE_VALIDATOR_PIN;
use crate::plan::PlanStep;
use crate::workflow::{
    GateCond, GateSpec, GateType, PhaseDef, PhaseExecutor, PhaseRole, StepOwner,
};

/// The garden QE security specialist `security_review` runs (DES-TEAMING-002 Q6, fixed in C1): the
/// frontmatter name of `skills/qe-security-test-engineer/SKILL.md` in wicked-garden.
pub const SECURITY_REVIEW_SKILL: &str = "wicked-garden-qe-security-test-engineer";
/// The skill domain-extraction's coverage judge runs (`domain_coverage`, X-MIG M6): the one skill
/// under which a self-verifying evaluator's code run is report-writing, not code work
/// ([`crate::plan`]'s `code_work_step`).
pub const DOMAIN_COVERAGE_SKILL: &str = "wicked-garden-domain-coverage";

/// The fifteen catalog ids, in the §8.3 table's order (the two walkthrough entries, WT-C1, sit
/// after `test`).
pub const CATALOG_IDS: [&str; 15] = [
    "understand",
    "test_plan",
    "design",
    "architecture",
    "build",
    "produce",
    "test",
    WALKTHROUGH_PLAN,
    WALKTHROUGH_REVIEW,
    "review",
    "critique",
    "security_review",
    "domain_coverage",
    "run",
    "deliver",
];

/// The walkthrough author (DES-walkthrough-proof §4.3, WT-C1): an EVALUATOR agent step that writes
/// one file, the storyline, into the run's declared author dir. Its posture is
/// [`crate::write_posture`]'s `DeliverableRoots` on a bound run, keyed off this id.
pub const WALKTHROUGH_PLAN: &str = "walkthrough_plan";
/// The walkthrough recorder + judge (DES-walkthrough-proof §4.3, WT-C1): a Tool step whose
/// command is FIXED here ([`WALKTHROUGH_RECORD_CMD`]) — the engine's own command, no seat. A step
/// may omit or restate the command, never change it (`plan::compose`: `tool_command_changed`).
pub const WALKTHROUGH_REVIEW: &str = "walkthrough_review";
/// The `walkthrough_review` entry's fixed command: garden's walkthrough tool, action `record`.
pub const WALKTHROUGH_RECORD_CMD: [&str; 4] = [
    "wicked-garden",
    "run",
    "scripts/demo/walkthrough.mjs",
    "record",
];

/// The phase catalog: fifteen entries, in [`CATALOG_IDS`] order. Built once; the slice is static.
pub fn catalog() -> &'static [PhaseDef] {
    static CATALOG: OnceLock<Vec<PhaseDef>> = OnceLock::new();
    CATALOG.get_or_init(build_catalog)
}

/// One catalog entry by id (`None` for an id the catalog does not define).
pub fn catalog_entry(id: &str) -> Option<&'static PhaseDef> {
    catalog().iter().find(|e| e.id == id)
}

/// One catalog entry as studio's phase picker reads it (`Core.catalog()`, crew `GET /catalog`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CatalogEntry {
    pub id: String,
    pub kind: StageKind,
    pub role: PhaseRole,
    pub gate: GateSpec,
    pub gate_type: Option<GateType>,
    pub executes_code: bool,
    /// `"agent"` or `"tool"` (a Tool step supplies its own command).
    pub executor: &'static str,
    pub validator_pin: Option<String>,
    /// The entry carries a validator pin (a step may not remove or swap it).
    pub pinned: bool,
    /// That pin is the evidence floor ([`EVIDENCE_FLOOR_PIN`]).
    pub evidence_floor: bool,
    /// The entry declares re-verified evidence (`PhaseDef::verified_evidence`): a step of it is
    /// an acceptance requirement of the run that contains it (`test`, `domain_coverage`).
    pub verified_evidence: bool,
    pub skill_ref: Option<String>,
    /// The entry's one-line description, when it has one (`null` for every entry today).
    pub description: Option<String>,
    /// (core#810, studio#617) The entry's worker pool (`PhaseDef::pool_size`, absent = 1): the
    /// most a plan step of it may ask for. A step may only lower it (`plan::compose`:
    /// `pool_raised`), so a plan editor offers 1 up to this and never more.
    pub pool: u8,
}

/// The catalog as [`CatalogEntry`] rows, in [`CATALOG_IDS`] order.
pub fn catalog_entries() -> Vec<CatalogEntry> {
    catalog()
        .iter()
        .map(|e| CatalogEntry {
            id: e.id.clone(),
            kind: e.kind,
            role: e.role,
            gate: e.gate,
            gate_type: e.gate_type,
            executes_code: e.executes_code,
            executor: if is_tool_entry(e) { "tool" } else { "agent" },
            validator_pin: e.validator_pin.clone(),
            pinned: e.validator_pin.is_some(),
            evidence_floor: e.validator_pin.as_deref() == Some(EVIDENCE_FLOOR_PIN),
            verified_evidence: e.verified_evidence,
            skill_ref: e.skill_ref.clone(),
            // No entry carries a description yet (the §8.3 table has none); the key is pinned so
            // a picker can render one the day an entry gains it.
            description: None,
            pool: e.pool_size(),
        })
        .collect()
}

/// `true` for an entry whose executor is a Tool (`run`, `deliver`, `walkthrough_review`): the only
/// entries a step may hand an `executor`.
pub fn is_tool_entry(entry: &PhaseDef) -> bool {
    matches!(entry.executor, PhaseExecutor::Tool { .. })
}

/// The built-in presets (DES-TEAMING-002 §8.4, seam C2): code data beside the catalog, written
/// to the store at boot by `crate::preset::seed_builtins` (`created_by: "builtin"`). Each is named
/// after the workflow it replaces, so a launch naming that id keeps launching; its steps are the
/// consumer's §11.2 mapping (`tests/fixtures/catalog/mappings.json`, pinned by a test).
///
/// Seeded here: `feature` (C2's acceptance), `bug` (M1), `chat` (M3), `onboarding` (M4), `migration` (M2),
/// `capture-learnings` (M7), `steering-author` (M8), `domain-extraction` (M6), `interactive-chat`,
/// `interactive-draft` and `interactive-edit` (M9), `mcp-server` (M12), `editor-plugin` (X3), `qe-author-tests` (M10) and `demo` (M9b,
/// which replaces `interactive-demo` and `interactive-demo-reauthor` rather than mapping them). Every other consumer's preset is added by its migration seam (§14 M1–M10),
/// which also deletes the def it replaces.
pub fn builtin_presets() -> Vec<(&'static str, Vec<PlanStep>)> {
    vec![
        ("bug", bug_preset()),
        ("capture-learnings", capture_learnings_preset()),
        ("chat", chat_preset()),
        ("demo", demo_preset()),
        ("domain-extraction", domain_extraction_preset()),
        ("editor-plugin", editor_plugin_preset()),
        ("feature", feature_preset()),
        (
            "interactive-chat",
            interactive_preset(
                include_str!("presets/interactive-chat.json"),
                "interactive-chat",
            ),
        ),
        (
            "interactive-draft",
            interactive_preset(
                include_str!("presets/interactive-draft.json"),
                "interactive-draft",
            ),
        ),
        (
            "interactive-edit",
            interactive_preset(
                include_str!("presets/interactive-edit.json"),
                "interactive-edit",
            ),
        ),
        ("mcp-server", mcp_server_preset()),
        ("migration", migration_preset()),
        ("onboarding", onboarding_preset()),
        ("qe-author-tests", qe_author_tests_preset()),
        ("steering-author", steering_author_preset()),
    ]
}

/// (QE acceptance, operator ruling 2026-10-10) The instruments a run of a BUILT-IN preset
/// requires, as a workflow's `required_instruments` declares them: the presets that make
/// application changes (`feature`, `bug`, `editor-plugin`, `mcp-server`, `migration`) require `qe_acceptance` on top of the
/// defaults. `None` ⇒ the defaults. Code data beside [`builtin_presets`], so a re-seed never drops it.
pub(crate) fn builtin_preset_instruments(name: &str) -> Option<Vec<String>> {
    matches!(
        name,
        "feature" | "bug" | "editor-plugin" | "mcp-server" | "migration"
    )
    .then(|| {
        [
            crate::assurance::DISTINCT_EVALUATOR,
            crate::assurance::JUDGE,
            crate::assurance::QE_ACCEPTANCE,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    })
}

/// One built-in preset's steps by name (`None` for a name that is not a built-in). Test fixtures.
#[cfg(test)]
pub(crate) fn builtin_preset(name: &str) -> Option<Vec<PlanStep>> {
    builtin_presets()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, steps)| steps)
}

/// `chat` (§11.2, seam M3): explore → `understand`. One read-only step: no creator step, so the
/// floor is empty, it is never high risk, and the PA does not scope it (X1 scopes creator plans
/// only). The launch is decided at once, `plan.proposed{by:"human", preset:"chat"}`.
fn chat_preset() -> Vec<PlanStep> {
    vec![PlanStep {
        catalog: "understand".to_string(),
        id: "explore".to_string(),
        ..PlanStep::default()
    }]
}

/// `onboarding` (§11.2, seam M4): index → annotate, both `run` (a Tool step), the repo bound per
/// run through the `{repo_root}` / `{code_graph_db}` placeholders (`crate::plan::bind_repo_paths`,
/// wicked-core#179 — never a baked path, FINDING-075). Tool-only: no seat, no creator step, an
/// empty floor, never high risk, and no PA scope step. `gate_type` is cleared, as today's def
/// carried none (it has no production reader).
///
/// # What this deliberately does NOT do
///
/// It does not produce `requirements_graph.json`. A third phase used to run `wicked-core
/// domain-graph` here, and it could never succeed: that command gates fail-closed on front-half
/// coverage == 1.0, and `wicked-estate clusters --annotate` is CLUSTERING — it annotates no symbol
/// with a requirement. On AutoGPT, after exactly these two steps, 28,885 of 28,885
/// behavior-bearing nodes were unaccounted, so every registration ended `sessionFailed` after the
/// work that mattered had succeeded (FINDING-068). `domain-graph` belongs to `domain-extraction`,
/// downstream of the `extract` + `coverage` phases that produce its precondition. Do not relax
/// that gate to make it pass here (DES-OUTGOV-001/005).
fn onboarding_preset() -> Vec<PlanStep> {
    use crate::workflow::{CODE_GRAPH_DB_TOKEN, REPO_ROOT_TOKEN};
    let tool = |id: &str, cmd: &[&str], after: Option<&str>| PlanStep {
        catalog: "run".to_string(),
        id: id.to_string(),
        gate_type: Some(None),
        depends_on: after.map(|a| vec![a.to_string()]),
        executor: Some(PhaseExecutor::Tool {
            cmd: cmd.iter().map(|a| a.to_string()).collect(),
        }),
        ..PlanStep::default()
    };
    vec![
        tool(
            "index",
            &[
                "wicked-estate",
                "index",
                REPO_ROOT_TOKEN,
                "--db",
                CODE_GRAPH_DB_TOKEN,
            ],
            None,
        ),
        tool(
            "annotate",
            &[
                "wicked-estate",
                "clusters",
                "--annotate",
                "--db",
                CODE_GRAPH_DB_TOKEN,
            ],
            Some("index"),
        ),
    ]
}

/// The wicked-garden skill every `demo` step runs: `skills/demo/SKILL.md`, actions plan / record /
/// review.
pub const DEMO_SKILL: &str = "wicked-garden-demo";

/// `demo` (M9b, studio#373): the wicked-garden demo skill's three actions as a team plan over the
/// DEMO ROOT — the one launch-declared write root (`LaunchSpec.extra_write_roots`) the launcher
/// names in the task. Every deliverable is relative, so it resolves inside that root
/// (`path_policy::missing_deliverables`) and nothing is written anywhere else.
///
/// - `plan` → `produce` (creator), gated `human_confirm`: THE PLAN GATE. The presenter script and
///   the chapter list are reviewed before anything records; approve moves on, `request_changes`
///   re-runs `plan` with the note.
/// - `record` → `produce` (creator): records every chapter as its own segment, stitches the MP4,
///   then writes the three contact sheets the review reads. The sheets are the recorder's work
///   product, so an evaluator never has to write (its posture is read-only).
/// - `review` → `critique` (evaluator), gated `human_confirm`: THE REVIEW GATE. A seat other than
///   the recorder's (evaluator ≠ creator) judges the sheets and reports one verdict per finding;
///   `request_changes` rewinds to `record`, which re-records only the named chapter.
///
/// The recorder is read-only against the app (the garden recorder refuses every non-GET request
/// to the target unless the storyline's target is disposable), and synthetic data is labelled.
fn demo_preset() -> Vec<PlanStep> {
    let confirm = Some(GateSpec::HumanConfirm {
        unconditional: false,
    });
    let files = |f: &[&str]| Some(f.iter().map(|s| s.to_string()).collect());
    vec![
        PlanStep {
            catalog: "produce".to_string(),
            id: "plan".to_string(),
            gate: confirm,
            skill_ref: Some(DEMO_SKILL.to_string()),
            instructions: Some(DEMO_PLAN_INSTRUCTIONS.to_string()),
            required_deliverables: files(&["script.md", "chapters.json", "storyline.mjs"]),
            ..PlanStep::default()
        },
        PlanStep {
            catalog: "produce".to_string(),
            id: "record".to_string(),
            skill_ref: Some(DEMO_SKILL.to_string()),
            instructions: Some(DEMO_RECORD_INSTRUCTIONS.to_string()),
            required_deliverables: files(&[
                "demo-video/demo.mp4",
                "demo-video/chapters.md",
                "demo-video/timings.json",
                "demo-video/segments",
                "review/chapters.png",
                "review/joins.png",
                "review/end.png",
            ]),
            depends_on: Some(vec!["plan".to_string()]),
            ..PlanStep::default()
        },
        PlanStep {
            catalog: "critique".to_string(),
            id: "review".to_string(),
            gate: confirm,
            skill_ref: Some(DEMO_SKILL.to_string()),
            instructions: Some(DEMO_REVIEW_INSTRUCTIONS.to_string()),
            depends_on: Some(vec!["record".to_string()]),
            ..PlanStep::default()
        },
    ]
}

/// The `plan` step's instructions. Every path is relative to the demo root the task names.
pub const DEMO_PLAN_INSTRUCTIONS: &str = "Run the wicked-garden-demo skill's `plan` action for \
the app and audience the task names. Work in the demo root the task names and write exactly: \
`script.md` (the presenter script from the skill's template: audience, pitch, run of show, case \
bank, measured vs estimate labelled), `chapters.json` (a JSON array, one object per chapter: \
{\"key\": \"NN-slug\", \"title\", \"blurb\", \"tags\": [..], \"resets\": [..]}) and \
`storyline.mjs` (the record action's storyline: `title: \"demo\"`, `baseUrl` = the app URL, one \
segment per chapter with the same keys). Rehearse against the running app READ-ONLY: navigate, \
hover, scroll, open panels and type into fields, but never submit, launch, approve, delete, \
create, cancel or save — show a control without pressing it. Say plainly in the script which \
data is synthetic and which systems are simulated. Write nothing outside the demo root.";

/// The `record` step's instructions.
pub const DEMO_RECORD_INSTRUCTIONS: &str = "Run the wicked-garden-demo skill's `record` action \
on `storyline.mjs` in the demo root the task names: `wicked-garden run scripts/demo/record.mjs \
<root>/storyline.mjs --out <root>/demo-video` (inside a crew run the launcher is \
\"$WICKED_GARDEN_ROOT/scripts/wicked-garden\"). It records every missing segment, then stitches \
`demo-video/demo.mp4` with chapters. The recorder is read-only against the app: it blocks every \
non-GET request and fails the segment `side_effect_blocked`. When a \
note asks to re-record one chapter, delete only `demo-video/segments/<key>/`, fix that segment \
in the storyline if the note says why, and run record.mjs with that key alone. Then write the \
review's contact sheets with the skill's contact_sheet.py: `--chapters --out \
<root>/review/chapters.png`, `--joins --out <root>/review/joins.png` and `--end --out \
<root>/review/end.png`, each on `demo-video/demo.mp4`. Write nothing outside the demo root.";

/// The `review` step's instructions (an evaluator: it reads and judges, it writes nothing).
pub const DEMO_REVIEW_INSTRUCTIONS: &str = "Run the wicked-garden-demo skill's `review` action \
on the recording in the demo root the task names: look at `review/chapters.png`, \
`review/joins.png` and `review/end.png`, and ffprobe `demo-video/demo.mp4` for chapters, size \
and codec. You judge work another seat recorded; change no file. Check every caption against \
its picture, the joins, the held closing card, and that synthetic data is labelled. Report \
every finding as `timestamp · chapter · what's wrong · verdict` (verdict: re-encode, \
re-record, or fix-app), then end with one fenced ```json block: {\"verdict\": \"accept\" or \
\"changes\", \"findings\": [{\"at\": \"m:ss\", \"chapter\": \"<key>\", \"issue\": \"..\", \
\"verdict\": \"re-encode|re-record|fix-app\"}]}.";

/// `feature` (§11.2): clarify → `understand` (gate raised to `human_confirm`); design → `design`;
/// build → `build`; adversarial-review → `review` (gate raised); test → `test`; review →
/// `critique`. The two bold cells (test and review on the evaluator role) come from the entries.
fn feature_preset() -> Vec<PlanStep> {
    let confirm = Some(GateSpec::HumanConfirm {
        unconditional: false,
    });
    let step = |catalog: &str, id: &str, after: Option<&str>| PlanStep {
        catalog: catalog.to_string(),
        id: id.to_string(),
        depends_on: after.map(|a| vec![a.to_string()]),
        ..PlanStep::default()
    };
    vec![
        PlanStep {
            gate: confirm,
            ..step("understand", "clarify", None)
        },
        step("design", "design", Some("clarify")),
        step("build", "build", Some("design")),
        PlanStep {
            gate: confirm,
            ..step("review", "adversarial-review", Some("build"))
        },
        step("test", "test", Some("build")),
        step("critique", "review", Some("test")),
    ]
}

/// `bug` (M1, §11.2): triage → `understand`; reproduce → `test_plan`; fix → `build` with the
/// retired-behaviour sweep instructions kept (DES-L9, BC-60); verify → `test`. The entries carry the
/// rest of the def: fix's evidence-floor pin, creator role and `executes_code`; verify's
/// `human_confirm_if` gate, pin, evaluator role and re-verified evidence. §11.3: floor fill adds
/// `review` at band ≥ 20 (the def had none), and the PA's `pa-scope` runs first.
fn bug_preset() -> Vec<PlanStep> {
    let step = |catalog: &str, id: &str, after: Option<&str>| PlanStep {
        catalog: catalog.to_string(),
        id: id.to_string(),
        depends_on: after.map(|a| vec![a.to_string()]),
        ..PlanStep::default()
    };
    vec![
        step("understand", "triage", None),
        step("test_plan", "reproduce", Some("triage")),
        PlanStep {
            instructions: Some(crate::workflow::BUG_FIX_SWEEP_INSTRUCTIONS.to_string()),
            ..step("build", "fix", Some("reproduce"))
        },
        step("test", "verify", Some("fix")),
    ]
}

/// The repo-learn skill every capture-learnings step runs (wicked-garden).
const REPO_LEARN_SKILL: &str = "wicked-garden-repo-learn";
/// capture-learnings' three phase instructions, moved verbatim from crew's def (M7).
const CAPTURE_CHURN_INSTRUCTIONS: &str = "Phase 1/3 CHURN: produce a ranked list of this repo's most actively-changed files and directories over the last ~12 months, plus the repo's real name (manifest or git remote) and parent project. Use the skill's bounded/sampled git-churn method — never stream the whole history. Do not read code deeply yet; the next phase targets these areas.";
const CAPTURE_HOTSPOTS_INSTRUCTIONS: &str = "Phase 2/3 HOTSPOTS: cross-reference the prior churn ranking with wicked-estate hotspot / blast-radius signals to find the load-bearing code, then READ it through the estate shim (`wicked-garden run scripts/_estate_client.py --readonly call …`, the skill's grounding path) to build a real technical understanding of how the system fits together — not a file listing. Reuse wicked-garden-search for the hotspot signals; follow the skill.";
const CAPTURE_CAPTURE_INSTRUCTIONS: &str = "Phase 3/3 CAPTURE: from the prior churn + hotspot understanding, submit durable learnings as estate proposals through the shim's `propose` per the skill's capture contract — BOTH memories (facts / how-it-works) and policies (enforced conventions), one proposal per item, tagged repo/project. Each is inert until human review; never include secrets or personal data. END with `wicked-capture-report {\"derived\": N, \"submitted\": M, \"failed\": K}` — always, even on a degrade or a legitimate 0 (which is acceptable).";

/// `capture-learnings` (M7): §11.2's row. churn, hotspots → `understand`; capture → `produce`, the
/// creator, which keeps the capture-report floor (`requires_capture_report`, BC-80) so a run whose
/// skill never submitted cannot report `completed`. Every step runs the repo-learn skill.
fn capture_learnings_preset() -> Vec<PlanStep> {
    let step = |catalog: &str, id: &str, instructions: &str, after: Option<&str>| PlanStep {
        catalog: catalog.to_string(),
        id: id.to_string(),
        instructions: Some(instructions.to_string()),
        skill_ref: Some(REPO_LEARN_SKILL.to_string()),
        depends_on: after.map(|a| vec![a.to_string()]),
        ..PlanStep::default()
    };
    vec![
        step("understand", "churn", CAPTURE_CHURN_INSTRUCTIONS, None),
        step(
            "understand",
            "hotspots",
            CAPTURE_HOTSPOTS_INSTRUCTIONS,
            Some("churn"),
        ),
        // capture's work is proposals to the estate store, never a file in the repo: it writes
        // nothing (core#649 option A), so its PA may scope the run `SCOPE {"touch":[]}`, which
        // scores 0, and an auto onboarding capture does not stop at plan approval (crew#552).
        PlanStep {
            requires_capture_report: Some(true),
            writes_nothing: Some(true),
            ..step(
                "produce",
                "capture",
                CAPTURE_CAPTURE_INSTRUCTIONS,
                Some("hotspots"),
            )
        },
    ]
}

/// `migration` (M2): §11.2's row. plan → `design` (gate raised to `human_confirm`); execute →
/// `build`; cutover → `build` with its UNCONDITIONAL human gate (the one gate the engagement dial
/// can never downgrade); verify → `test`; cleanup → `build` with no gate type. The bold cells are
/// the catalog's: cutover and cleanup gain the evidence-floor pin and the creator role, and cleanup
/// gains `executes_code` (decision 2026-09-24: it removes the old path, so it is code work).
fn migration_preset() -> Vec<PlanStep> {
    let step = |catalog: &str, id: &str, after: Option<&str>| PlanStep {
        catalog: catalog.to_string(),
        id: id.to_string(),
        depends_on: after.map(|a| vec![a.to_string()]),
        ..PlanStep::default()
    };
    vec![
        PlanStep {
            gate: Some(GateSpec::HumanConfirm {
                unconditional: false,
            }),
            ..step("design", "plan", None)
        },
        step("build", "execute", Some("plan")),
        PlanStep {
            gate: Some(GateSpec::HumanConfirm {
                unconditional: true,
            }),
            ..step("build", "cutover", Some("execute"))
        },
        step("test", "verify", Some("cutover")),
        PlanStep {
            gate_type: Some(None),
            ..step("build", "cleanup", Some("verify"))
        },
    ]
}

/// The interactive document presets (M9: `interactive-chat`, `interactive-draft`,
/// `interactive-edit`): §11.2's rows, moved from crew's defs as data (`src/presets/<name>.json`,
/// in the form crew registered them with the draft skill held, `withDraftSkill(<def>, true)`;
/// pinned to the C1 mapping by a test). chat: understand → `understand`, revise → `produce`;
/// draft: draft → `produce` (crew folded the outline into its one phase); edit: edit → `produce`.
/// Every agent step runs `wicked-garden-draft`, the document quality floor. They are repo-less creator plans, so
/// the PA rates their RISK first (§11.3): a routine internal draft proceeds in auto mode, a
/// high-stakes one pauses.
fn interactive_preset(json: &str, name: &str) -> Vec<PlanStep> {
    let plan: crate::plan::PlanSteps = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("src/presets/{name}.json is a valid plan: {e}"));
    plan.steps
}

/// `mcp-server` (M12, DES-W7-M12 on core#649): the def's nine phases as data
/// (`src/presets/mcp-server.json`, pinned to its mapping by a test). scope, source-discovery →
/// `understand`; design → `design`; build → `build`; test → `test` (the bold cell: role neutral →
/// evaluator); security-review and observability-review → `review`, each keeping its specialist
/// skill and its raised gate (`review` leaves the skill unset; the catalog's `security_review` fixes
/// its own, so the MCP review is a `review` and a high band's floor adds the generic one beside
/// it); install-plan and install → `run` with their Tool commands, install gated `consent_before`.
/// Phase ids are kept: crew keys delivery on `install-plan` / `install`.
fn mcp_server_preset() -> Vec<PlanStep> {
    let plan: crate::plan::PlanSteps =
        serde_json::from_str(include_str!("presets/mcp-server.json"))
            .expect("src/presets/mcp-server.json is a valid plan");
    plan.steps
}

/// `editor-plugin` (operator ruling X3, 2026-10-10): make an artifact editor plugin for wicked-studio
/// as data (`src/presets/editor-plugin.json`), modelled on `mcp-server`. scope → `understand`; design →
/// `design` (the contract against DES-EDITOR-PLUGINS-001); build → `build` (the
/// `wicked-garden-editor-scaffold` skill); test → `test`, which runs the editor conformance harness and is
/// the run's QE phase (`qe_acceptance` required, as for every app-change preset); security-review →
/// `review` keeping the platform specialist; install-plan and install → `run` with their Tool
/// commands, install gated `consent_before`. The ids `install-plan` / `install` are the ones a
/// delivering run's deliver goes before ([`crate::plan_gate`]).
fn editor_plugin_preset() -> Vec<PlanStep> {
    let plan: crate::plan::PlanSteps =
        serde_json::from_str(include_str!("presets/editor-plugin.json"))
            .expect("src/presets/editor-plugin.json is a valid plan");
    plan.steps
}

/// `qe-author-tests` (M10): §11.2's row, moved from crew's def as data
/// (`src/presets/qe-author-tests.json`, pinned to the C1 mapping by a test). recon → `understand`
/// (gate type strategy, the QE skill's plan action); author → `build` (the creator that writes
/// behaviour tests, skill `wicked-garden-qe`); verify → `run`, the deterministic Tool step that
/// runs every produced test under the repository's own harness, with the evidence-floor pin
/// ADDED; review → `review` (gate raised to `human_confirm_if`). The verify script is long and
/// lives in the data file, not in Rust string literals.
fn qe_author_tests_preset() -> Vec<PlanStep> {
    let plan: crate::plan::PlanSteps =
        serde_json::from_str(include_str!("presets/qe-author-tests.json"))
            .expect("src/presets/qe-author-tests.json is a valid plan");
    plan.steps
}

/// steering-author's two phase instructions, moved verbatim from crew's def (M8).
const STEERING_ANALYZE_INSTRUCTIONS: &str = "Read the operator intent and every file or directory listed in the problem statement. Identify candidate steering rules: durable, prescriptive statements a coding agent must follow, each classified into one steering type (architecture, development, security, testing, operations, compliance, design-ux). For each candidate note the statement, steering type, severity, and the evidence in the source material. Analysis only — do not write any rule to any store, and do not emit final rule JSON yet.";
const STEERING_PROPOSE_INSTRUCTIONS: &str = "From the prior analysis, emit the PROPOSED steering rules as one JSON array. Each entry is a conformance-rule object: id (PAT-<digits> for rule_type \"pattern\", POL-<digits> for \"policy\"), rule_type, statement, severity (info|warn|error|critical), confidence (a NUMBER 0..1), steering_type (default to the type named in the problem statement), provenance {\"source\":\"chat\"}, and — only where the source material supports them — the enforcement fields applies_to (array of phase tokens or globs), excludes, weight, obligations (array of strings), criteria (ONE string, never a list). Omit targets, effect and trigger unless you can express them in the store schema exactly: targets is a {language, layer, framework} facet OBJECT (never a file list — files belong in applies_to), and trigger is a structured condition object (never prose). Put that JSON array in your reply, ONCE, as a single ```json fenced block (the whole array, valid JSON): your reply IS the proposal — crew reads the array from it when the human approves. Do not write it to any file and do not create files for it. This output is a PROPOSAL for the human gate: rules land in the governance store only after approval, written crew-side — do not write any rule to any store yourself.";

/// `steering-author` (M8): §11.2's row. analyze → `understand`; propose → `produce` (the bold
/// cell: kind recon → build), keeping its UNCONDITIONAL human gate. That gate is the TH-12
/// propose-as-gate: crew lands the approved rules from the propose unit's reply (crew#388, #789),
/// and the run itself writes nothing to the store.
fn steering_author_preset() -> Vec<PlanStep> {
    vec![
        PlanStep {
            catalog: "understand".to_string(),
            id: "analyze".to_string(),
            instructions: Some(STEERING_ANALYZE_INSTRUCTIONS.to_string()),
            ..PlanStep::default()
        },
        PlanStep {
            catalog: "produce".to_string(),
            id: "propose".to_string(),
            instructions: Some(STEERING_PROPOSE_INSTRUCTIONS.to_string()),
            gate: Some(GateSpec::HumanConfirm {
                unconditional: true,
            }),
            // The proposal is the reply; crew lands the approved rules, so the run writes no
            // file (core#649 option A: an empty SCOPE scores 0, not a fail-closed 100).
            writes_nothing: Some(true),
            depends_on: Some(vec!["analyze".to_string()]),
            ..PlanStep::default()
        },
    ]
}

/// `domain-extraction` (M6): §11.2's row. survey, analyze → `understand` (no gate type);
/// extract → `produce` (the bold cell: kind recon → build); coverage → `domain_coverage`, which
/// carries `COVERAGE_VALIDATOR_PIN` as data (the step restates it, a no-op), with `executes_code`
/// raised and its report as the deliverable; domain-graph → `run`, the Tool step that builds the
/// requirements graph, behind a `human_confirm` gate. The skills are the domain family's.
fn domain_extraction_preset() -> Vec<PlanStep> {
    use crate::workflow::CODE_GRAPH_DB_TOKEN;
    let step = |catalog: &str, id: &str, after: Option<&str>| PlanStep {
        catalog: catalog.to_string(),
        id: id.to_string(),
        depends_on: after.map(|a| vec![a.to_string()]),
        ..PlanStep::default()
    };
    vec![
        PlanStep {
            gate_type: Some(None),
            skill_ref: Some("wicked-garden-domain".to_string()),
            ..step("understand", "survey", None)
        },
        PlanStep {
            gate_type: Some(None),
            skill_ref: Some("wicked-garden-domain".to_string()),
            ..step("understand", "analyze", Some("survey"))
        },
        PlanStep {
            skill_ref: Some("wicked-garden-domain-extractor".to_string()),
            ..step("produce", "extract", Some("analyze"))
        },
        PlanStep {
            executes_code: Some(true),
            required_deliverables: Some(vec!["coverage-report.json".to_string()]),
            skill_ref: Some(DOMAIN_COVERAGE_SKILL.to_string()),
            validator_pin: Some(Some(COVERAGE_VALIDATOR_PIN.to_string())),
            ..step("domain_coverage", "coverage", Some("extract"))
        },
        PlanStep {
            kind: Some(StageKind::Build),
            gate_type: Some(Some(GateType::Strategy)),
            gate: Some(GateSpec::HumanConfirm {
                unconditional: false,
            }),
            executor: Some(PhaseExecutor::Tool {
                cmd: [
                    "wicked-core",
                    "domain-graph",
                    "--db",
                    CODE_GRAPH_DB_TOKEN,
                    "--out",
                    "requirements_graph.json",
                ]
                .map(str::to_string)
                .to_vec(),
            }),
            ..step("run", "domain-graph", Some("coverage"))
        },
    ]
}

fn build_catalog() -> Vec<PhaseDef> {
    use GateType::{Execution, Strategy, Value};
    use PhaseRole::{Creator, Evaluator, Neutral};
    use StageKind::{Build, Recon, Review, Test};
    let auto = GateSpec::Auto;
    let floor = || Some(EVIDENCE_FLOOR_PIN.to_string());
    let tool = || PhaseExecutor::Tool { cmd: Vec::new() };
    vec![
        entry("understand", Recon, Neutral, auto, Value, None, false),
        entry("test_plan", Test, Neutral, auto, Value, None, false),
        entry("design", Recon, Neutral, auto, Strategy, None, false),
        entry("architecture", Recon, Neutral, auto, Strategy, None, false),
        entry("build", Build, Creator, auto, Execution, floor(), true),
        entry("produce", Build, Creator, auto, Value, None, false),
        PhaseDef {
            // The one entry that declares re-verified evidence: its pin is what re-verifies it.
            verified_evidence: true,
            ..entry(
                "test",
                Test,
                Evaluator,
                GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                Execution,
                floor(),
                false,
            )
        },
        PhaseDef {
            // WT-C1: the walkthrough author. Dormant — no preset uses it (N5).
            skill_ref: Some(DEMO_SKILL.to_string()),
            ..entry(
                WALKTHROUGH_PLAN,
                Test,
                Evaluator,
                auto,
                Execution,
                Some(WALKTHROUGH_LINT_PIN.to_string()),
                false,
            )
        },
        PhaseDef {
            // WT-C1: the engine-run recorder + judge. Its pin reads the sealed result, so it is
            // what re-verifies the evidence; the command is fixed (a step may only restate it).
            verified_evidence: true,
            executor: PhaseExecutor::Tool {
                cmd: WALKTHROUGH_RECORD_CMD
                    .iter()
                    .map(|a| a.to_string())
                    .collect(),
            },
            ..entry(
                WALKTHROUGH_REVIEW,
                Test,
                Neutral,
                auto,
                Execution,
                Some(WALKTHROUGH_RESULT_PIN.to_string()),
                false,
            )
        },
        entry("review", Review, Evaluator, auto, Execution, floor(), false),
        entry("critique", Review, Evaluator, auto, Execution, None, false),
        PhaseDef {
            skill_ref: Some(SECURITY_REVIEW_SKILL.to_string()),
            ..entry(
                "security_review",
                Review,
                Evaluator,
                auto,
                Execution,
                floor(),
                false,
            )
        },
        PhaseDef {
            // domain-extraction's coverage judge: the `test` shape with the coverage pin as data,
            // and re-verified evidence like `test` (its pin is what re-verifies it).
            verified_evidence: true,
            ..entry(
                "domain_coverage",
                Test,
                Evaluator,
                GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                Execution,
                Some(COVERAGE_VALIDATOR_PIN.to_string()),
                false,
            )
        },
        PhaseDef {
            executor: tool(),
            ..entry("run", Recon, Neutral, auto, Value, None, false)
        },
        PhaseDef {
            executor: tool(),
            ..entry("deliver", Build, Neutral, auto, Execution, None, false)
        },
    ]
}

fn entry(
    id: &str,
    kind: StageKind,
    role: PhaseRole,
    gate: GateSpec,
    gate_type: GateType,
    validator_pin: Option<String>,
    executes_code: bool,
) -> PhaseDef {
    PhaseDef {
        requires_capture_report: false,
        id: id.to_string(),
        kind,
        instructions: None,
        gate_type: Some(gate_type),
        gate,
        executes_code,
        budget_secs: None,
        pool: None,
        verified_evidence: false,
        required_deliverables: Vec::new(),
        depends_on: Vec::new(),
        role,
        skill_ref: None,
        allowed_skills: Vec::new(),
        validator_pin,
        executor: PhaseExecutor::Agent,
        owner: StepOwner::Pa,
        catalog: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §8.3 table, cell by cell, as fixed values (not re-derived from the builder).
    #[test]
    fn the_catalog_is_the_fifteen_entries_of_the_table() {
        let got: Vec<_> = catalog()
            .iter()
            .map(|e| {
                (
                    e.id.as_str(),
                    serde_json::to_value(e.kind).unwrap(),
                    serde_json::to_value(e.role).unwrap(),
                    serde_json::to_value(e.gate).unwrap(),
                    serde_json::to_value(e.gate_type).unwrap(),
                    e.validator_pin.as_deref(),
                    e.executes_code,
                    serde_json::to_value(&e.executor).unwrap()["type"].clone(),
                    e.skill_ref.as_deref(),
                )
            })
            .collect();
        let j = |s: &str| serde_json::Value::String(s.to_string());
        let hci = serde_json::json!({"human_confirm_if": "verdict_not_pass"});
        let f = Some("e2e7af1db9e48454");
        #[rustfmt::skip]
        let want = vec![
            ("understand", j("recon"), j("neutral"), j("auto"), j("value"), None, false, j("agent"), None),
            ("test_plan", j("test"), j("neutral"), j("auto"), j("value"), None, false, j("agent"), None),
            ("design", j("recon"), j("neutral"), j("auto"), j("strategy"), None, false, j("agent"), None),
            ("architecture", j("recon"), j("neutral"), j("auto"), j("strategy"), None, false, j("agent"), None),
            ("build", j("build"), j("creator"), j("auto"), j("execution"), f, true, j("agent"), None),
            ("produce", j("build"), j("creator"), j("auto"), j("value"), None, false, j("agent"), None),
            ("test", j("test"), j("evaluator"), hci.clone(), j("execution"), f, false, j("agent"), None),
            ("walkthrough_plan", j("test"), j("evaluator"), j("auto"), j("execution"), Some("1aa3f15487018f68"), false, j("agent"), Some("wicked-garden-demo")),
            ("walkthrough_review", j("test"), j("neutral"), j("auto"), j("execution"), Some("cd95e6e0acdb4d8b"), false, j("tool"), None),
            ("review", j("review"), j("evaluator"), j("auto"), j("execution"), f, false, j("agent"), None),
            ("critique", j("review"), j("evaluator"), j("auto"), j("execution"), None, false, j("agent"), None),
            (
                "security_review",
                j("review"),
                j("evaluator"),
                j("auto"),
                j("execution"),
                f,
                false,
                j("agent"),
                Some("wicked-garden-qe-security-test-engineer"),
            ),
            ("domain_coverage", j("test"), j("evaluator"), hci.clone(), j("execution"), Some("bfe4020a365c598b"), false, j("agent"), None),
            ("run", j("recon"), j("neutral"), j("auto"), j("value"), None, false, j("tool"), None),
            ("deliver", j("build"), j("neutral"), j("auto"), j("execution"), None, false, j("tool"), None),
        ];
        assert_eq!(got, want);
        let ids: Vec<_> = catalog().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, CATALOG_IDS);
    }

    /// (T8, `Core.catalog()`) Every entry as studio's phase picker reads it, in catalog order,
    /// with the exact key set pinned: `pinned` = the entry carries a validator pin, and
    /// `evidence_floor` = that pin is the evidence floor, `verified_evidence` = a step of the entry
    /// is an acceptance requirement of its run. `description` is the entry's own text
    /// (`null`: no catalog entry carries one today).
    #[test]
    fn catalog_entries_are_the_catalog_as_the_picker_reads_it() {
        let v = serde_json::to_value(catalog_entries()).unwrap();
        let entries = v.as_array().unwrap();
        let ids: Vec<_> = entries.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, CATALOG_IDS);
        let keys: Vec<&str> = entries[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "description",
                "evidence_floor",
                "executes_code",
                "executor",
                "gate",
                "gate_type",
                "id",
                "kind",
                "pinned",
                "pool",
                "role",
                "skill_ref",
                "validator_pin",
                "verified_evidence"
            ]
        );
        let by = |id: &str| entries.iter().find(|e| e["id"] == id).unwrap().clone();
        assert_eq!(
            by("build"),
            serde_json::json!({
                "id": "build", "kind": "build", "role": "creator", "gate": "auto",
                "gate_type": "execution", "executes_code": true, "executor": "agent",
                "validator_pin": EVIDENCE_FLOOR_PIN, "pinned": true, "evidence_floor": true,
                "verified_evidence": false, "skill_ref": null, "description": null,
                "pool": 1
            })
        );
        // (core#810, studio#617) Every shipped entry is a pool of 1 today: the key is pinned so a
        // plan editor can offer a lower pool the day an entry declares one.
        assert!(entries.iter().all(|e| e["pool"] == 1));
        let cov = by("domain_coverage");
        assert_eq!(
            (cov["pinned"].clone(), cov["evidence_floor"].clone()),
            (true.into(), false.into())
        );
        assert_eq!(
            cov["gate"],
            serde_json::json!({"human_confirm_if": "verdict_not_pass"})
        );
        let run = by("run");
        assert_eq!(run["executor"], "tool");
        assert_eq!(
            (run["pinned"].clone(), run["evidence_floor"].clone()),
            (false.into(), false.into())
        );
        assert_eq!(by("security_review")["skill_ref"], SECURITY_REVIEW_SKILL);
        // `verified_evidence`: the entries whose step is an acceptance requirement of its run.
        let verified: Vec<_> = entries
            .iter()
            .filter(|e| e["verified_evidence"] == true)
            .map(|e| e["id"].as_str().unwrap())
            .collect();
        assert_eq!(verified, ["test", WALKTHROUGH_REVIEW, "domain_coverage"]);
    }

    fn composed(name: &str) -> crate::workflow::WorkflowDef {
        let steps = builtin_preset(name).unwrap_or_else(|| panic!("no built-in preset `{name}`"));
        crate::plan::compose(
            catalog(),
            &crate::plan::PlanSteps {
                steps,
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("built-in `{name}` composes: {e}"))
    }

    /// M10: the built-in `qe-author-tests` composes to crew's def: the author is the code-writing
    /// creator on the QE skill; verify is the Tool step running crew's verify script (the summary
    /// marker crew reads) with the evidence-floor pin; review is the evaluator gate.
    #[test]
    fn qe_author_tests_composes_with_its_verify_tool_and_floor() {
        let def = composed("qe-author-tests");
        let by = |id: &str| def.phases.iter().find(|p| p.id == id).unwrap();
        let author = by("author");
        assert!(author.executes_code && author.role == PhaseRole::Creator);
        assert_eq!(author.skill_ref.as_deref(), Some("wicked-garden-qe"));
        let verify = by("verify");
        assert_eq!(verify.validator_pin.as_deref(), Some(EVIDENCE_FLOOR_PIN));
        // The acceptance declaration crew's def carried (crew `qe/acceptance.ts` reads it).
        assert!(verify.verified_evidence);
        match &verify.executor {
            PhaseExecutor::Tool { cmd } => {
                assert_eq!(cmd.first().map(String::as_str), Some("bash"));
                assert!(cmd.last().unwrap().contains("QE-VERIFY-SUMMARY:"));
            }
            other => panic!("verify is a Tool step, got {other:?}"),
        }
        assert_eq!(by("review").role, PhaseRole::Evaluator);
    }

    /// M6: the built-in `domain-extraction` composes to the def the shipped JSON describes (the C1
    /// mapping pins every field; this names the ones the run depends on). coverage carries the
    /// coverage validator's pin from its catalog entry, so no installed copy can drift
    /// (FINDING-080); extract is a `produce` creator; domain-graph is the Tool step behind a human
    /// gate, with the code graph placeholder the planner binds per run.
    #[test]
    fn domain_extraction_composes_with_its_coverage_pin_and_graph_tool() {
        let def = composed("domain-extraction");
        let by = |id: &str| def.phases.iter().find(|p| p.id == id).unwrap();
        let coverage = by("coverage");
        assert_eq!(
            coverage.validator_pin.as_deref(),
            Some(COVERAGE_VALIDATOR_PIN)
        );
        assert!(coverage.executes_code && coverage.verified_evidence);
        assert_eq!(coverage.required_deliverables, ["coverage-report.json"]);
        let extract = by("extract");
        assert_eq!(
            (extract.kind, extract.role),
            (StageKind::Build, PhaseRole::Creator)
        );
        let graph = by("domain-graph");
        match &graph.executor {
            PhaseExecutor::Tool { cmd } => {
                assert!(cmd
                    .iter()
                    .any(|a| a == crate::workflow::CODE_GRAPH_DB_TOKEN))
            }
            other => panic!("domain-graph is a Tool step, got {other:?}"),
        }
        assert_eq!(
            graph.gate,
            GateSpec::HumanConfirm {
                unconditional: false
            }
        );
    }

    /// M3/M4: `chat` (read-only) and `onboarding` (tool-only) have no creator step, so the PA does
    /// not scope them (X1: `needs_pa_scope` is false with no `touch`), and no evaluator either, so
    /// evaluator ≠ creator has no unit to move off the creator seat.
    #[test]
    fn chat_and_onboarding_are_neither_creator_nor_evaluator_plans() {
        for name in ["chat", "onboarding"] {
            let plan = crate::plan::PlanSteps {
                steps: builtin_preset(name).unwrap(),
                ..Default::default()
            };
            assert!(!plan.has_creator(), "{name} has a creator step");
            assert!(
                !crate::plan_gate::needs_pa_scope(&plan),
                "{name} would be scoped"
            );
            let def = composed(name);
            assert!(
                def.phases.iter().all(|p| p.role == PhaseRole::Neutral),
                "{name}: every step is neutral"
            );
        }
        let chat = composed("chat");
        assert_eq!(
            chat.phases
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["explore"]
        );
        assert!(!chat.phases[0].executes_code, "chat is read-only");
        assert!(composed("onboarding").phases.iter().all(is_tool_entry));
    }

    /// Onboarding runs the two deterministic steps and stops (FINDING-068), moved here from the
    /// deleted `onboarding_def` with the def (DES-TEAMING-002 M4). Stated as "no step shells out to
    /// domain-graph", because the defect is that COMMAND's unmeetable coverage precondition; and
    /// every step reads the graph by placeholder, never a baked path (FINDING-075).
    #[test]
    fn onboarding_runs_only_what_it_can_actually_finish() {
        let def = composed("onboarding");
        assert_eq!(
            def.phases.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["index", "annotate"]
        );
        for phase in &def.phases {
            let PhaseExecutor::Tool { cmd } = &phase.executor else {
                panic!("onboarding step `{}` is not a tool step", phase.id);
            };
            assert!(
                !cmd.iter().any(|a| a == "domain-graph"),
                "onboarding step `{}` runs `{}` — see FINDING-068",
                phase.id,
                cmd.join(" ")
            );
            let db = cmd.iter().position(|a| a == "--db").expect("names --db");
            assert_eq!(cmd[db + 1], crate::workflow::CODE_GRAPH_DB_TOKEN);
            assert!(!cmd.iter().any(|a| a.starts_with('/')), "{cmd:?}");
        }
        let PhaseExecutor::Tool { cmd } = &def.phases[0].executor else {
            unreachable!()
        };
        assert!(cmd.iter().any(|a| a == crate::workflow::REPO_ROOT_TOKEN));
        assert_eq!(def.phases[1].depends_on, ["index"]);
    }

    /// The evidence floor moved onto the catalog (DES-TEAMING-002 §10): exactly the entries whose
    /// evidence is a code change carry it — the code-writing `build` and the code-judging `test`,
    /// `review` and `security_review` — and nothing that writes or judges prose does.
    #[test]
    fn the_evidence_floor_sits_on_the_code_entries_only() {
        let floored: Vec<_> = catalog()
            .iter()
            .filter(|e| e.validator_pin.as_deref() == Some(EVIDENCE_FLOOR_PIN))
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(floored, ["build", "test", "review", "security_review"]);
        // The only other pin is domain_coverage's coverage pin.
        let other: Vec<_> = catalog()
            .iter()
            .filter_map(|e| match e.validator_pin.as_deref() {
                None | Some(EVIDENCE_FLOOR_PIN) => None,
                Some(pin) => Some((e.id.as_str(), pin)),
            })
            .collect();
        // The other pins: the two walkthrough pins (WT-C1) and domain_coverage's coverage pin.
        assert_eq!(
            other,
            [
                (WALKTHROUGH_PLAN, WALKTHROUGH_LINT_PIN),
                (WALKTHROUGH_REVIEW, WALKTHROUGH_RESULT_PIN),
                ("domain_coverage", COVERAGE_VALIDATOR_PIN)
            ]
        );
    }

    /// WT-C1 (DES-walkthrough-proof §4.3): `walkthrough_review` is a Tool entry whose command is
    /// FIXED in the catalog (the engine runs garden's recorder + judge; no seat), and only it
    /// re-verifies evidence among the two walkthrough entries.
    #[test]
    fn the_walkthrough_entries_carry_a_fixed_record_command_and_verified_evidence() {
        let review = catalog_entry("walkthrough_review").expect("walkthrough_review entry");
        assert_eq!(
            serde_json::to_value(&review.executor).unwrap(),
            serde_json::json!({"type": "tool", "cmd": ["wicked-garden", "run", "scripts/demo/walkthrough.mjs", "record"]})
        );
        assert!(review.verified_evidence);
        let plan = catalog_entry("walkthrough_plan").expect("walkthrough_plan entry");
        assert!(!plan.verified_evidence);
        assert!(
            !plan.executes_code,
            "the author writes a storyline, never the tree"
        );
    }

    /// WT-C1 (N5, dormant): no built-in preset references the walkthrough entries — a walkthrough
    /// reaches a run only when a PA or a user adds one.
    #[test]
    fn no_builtin_preset_references_the_walkthrough_entries() {
        for (name, steps) in builtin_presets() {
            for step in steps {
                assert!(
                    !step.catalog.starts_with("walkthrough_"),
                    "preset {name} step {} uses {}",
                    step.id,
                    step.catalog
                );
            }
        }
    }

    /// WT-C1: the recorder's write escape hatch is gone (garden WT-G1 deletes the variable), so the
    /// demo `record` instruction no longer names it.
    #[test]
    fn the_demo_record_instruction_does_not_name_the_deleted_env_var() {
        assert!(
            !DEMO_RECORD_INSTRUCTIONS.contains("DEMO_ALLOW_WRITES"),
            "{DEMO_RECORD_INSTRUCTIONS}"
        );
    }
}
