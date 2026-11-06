//! The phase catalog (DES-TEAMING-002 §8.3): the one place every phase TYPE a plan may use is
//! defined. Data and nothing else — no workflow file, no per-surface copy.
//!
//! Each entry IS a [`PhaseDef`], so every existing control keeps working through the mechanism
//! that already enforces it: `plan_from_def` copies `gate` / `role` / `owner` onto the unit,
//! `attach_pinned_validators` attaches each `validator_pin`, and the fence reads `role`. A plan's
//! steps are composed onto these entries by [`crate::plan::compose`], which lets a step only make
//! its entry STRICTER (raise the gate, add a pin to an unpinned entry, set `executes_code`) and
//! never touch `role` or a pin the entry carries. The one exception is the `answer_only` output
//! declaration (crew#1311), admitted only where nothing in the run builds on or reviews the step.
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

/// The seventeen catalog ids, in the §8.3 table's order (the two walkthrough entries, WT-C1, sit
/// after `test`; the two demo Tools, DES-DEMO-RECORD-001, close the table).
pub const CATALOG_IDS: [&str; 17] = [
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
    crate::demo_record::PROBE_CATALOG_ID,
    crate::demo_record::RECORD_CATALOG_ID,
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

/// The phase catalog: seventeen entries, in [`CATALOG_IDS`] order. Built once; the slice is static.
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
/// Seeded here: `analyze` (crew#1157), `feature` (C2's acceptance), `bug` (M1), `chat` (M3), `onboarding` (M4), `migration` (M2),
/// `capture-learnings` (M7), `steering-author` (M8), `domain-extraction` (M6), `interactive-chat`,
/// `interactive-draft` and `interactive-edit` (M9), `mcp-server` (M12), `editor-plugin` (X3), `learn` (DES-learn-workflow), `qe-author-tests` (M10) and `demo` (M9b,
/// which replaces `interactive-demo` and `interactive-demo-reauthor` rather than mapping them), and the
/// workflow-catalog gap fills `docs`, `validate-docs`, `test-plan` and `test-analysis` (new workflows as
/// data over existing catalog entries and garden skills, `src/presets/<name>.json`; no §11.2 row), and the
/// Apps presets `apps-turn` and `apps-publish` (DES-APPS-001 K0; fixed plans, [`crate::apps`]). Every other consumer's preset is added by its migration seam (§14 M1–M10),
/// which also deletes the def it replaces.
pub fn builtin_presets() -> Vec<(&'static str, Vec<PlanStep>)> {
    vec![
        ("analyze", analyze_preset()),
        (
            "analyze-codebase",
            catalog_preset(
                include_str!("presets/analyze-codebase.json"),
                "analyze-codebase",
            ),
        ),
        (
            crate::apps::APPS_PUBLISH,
            apps_preset(include_str!("presets/apps-publish.json"), "apps-publish"),
        ),
        (
            crate::apps::APPS_TURN,
            apps_preset(include_str!("presets/apps-turn.json"), "apps-turn"),
        ),
        ("bug", bug_preset()),
        ("capture-learnings", capture_learnings_preset()),
        ("chat", chat_preset()),
        (
            "compliance-review",
            catalog_preset(
                include_str!("presets/compliance-review.json"),
                "compliance-review",
            ),
        ),
        (
            "decide",
            catalog_preset(include_str!("presets/decide.json"), "decide"),
        ),
        ("demo", demo_preset()),
        ("demo-human-review", demo_human_review_preset()),
        (
            "docs",
            interactive_preset(include_str!("presets/docs.json"), "docs"),
        ),
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
        (
            "investigate-incident",
            catalog_preset(
                include_str!("presets/investigate-incident.json"),
                "investigate-incident",
            ),
        ),
        ("learn", learn_preset()),
        (
            "map-requirements",
            catalog_preset(
                include_str!("presets/map-requirements.json"),
                "map-requirements",
            ),
        ),
        ("mcp-server", mcp_server_preset()),
        ("migration", migration_preset()),
        (
            "modernize",
            catalog_preset(include_str!("presets/modernize.json"), "modernize"),
        ),
        ("onboarding", onboarding_preset()),
        (
            "plan",
            catalog_preset(include_str!("presets/plan.json"), "plan"),
        ),
        (
            "privacy-review",
            catalog_preset(
                include_str!("presets/privacy-review.json"),
                "privacy-review",
            ),
        ),
        ("qe-author-tests", qe_author_tests_preset()),
        (
            "reconstruct",
            catalog_preset(include_str!("presets/reconstruct.json"), "reconstruct"),
        ),
        (
            "refactor",
            catalog_preset(include_str!("presets/refactor.json"), "refactor"),
        ),
        (
            "release",
            catalog_preset(include_str!("presets/release.json"), "release"),
        ),
        (
            "review-change",
            catalog_preset(include_str!("presets/review-change.json"), "review-change"),
        ),
        (
            "risk-analysis",
            catalog_preset(include_str!("presets/risk-analysis.json"), "risk-analysis"),
        ),
        (
            "scaffold",
            catalog_preset(include_str!("presets/scaffold.json"), "scaffold"),
        ),
        (
            "security-review",
            catalog_preset(
                include_str!("presets/security-review.json"),
                "security-review",
            ),
        ),
        ("steering-author", steering_author_preset()),
        (
            "suite-health",
            catalog_preset(include_str!("presets/suite-health.json"), "suite-health"),
        ),
        (
            "test-analysis",
            interactive_preset(include_str!("presets/test-analysis.json"), "test-analysis"),
        ),
        (
            "test-plan",
            interactive_preset(include_str!("presets/test-plan.json"), "test-plan"),
        ),
        (
            "validate-docs",
            interactive_preset(include_str!("presets/validate-docs.json"), "validate-docs"),
        ),
    ]
}

/// (QE acceptance, operator ruling 2026-10-10) The instruments a run of a BUILT-IN preset
/// requires, as a workflow's `required_instruments` declares them: the presets that make
/// application changes (`feature`, `bug`, `editor-plugin`, `mcp-server`, `migration`) and the Apps
/// publish (`apps-publish`, DES-APPS-001 §9.3) require `qe_acceptance` on top of the defaults. `None` ⇒ the defaults. Code data beside [`builtin_presets`], so a re-seed never drops it.
pub fn builtin_preset_instruments(name: &str) -> Option<Vec<String>> {
    // (DES-APPS-001 §9.2) An Apps turn has no QE unit: its instrument is the review on a seat
    // distinct from the builder ("apps-turn requires the distinct evaluator"). `judge` is not
    // required: the review's pinned judge must be distinct from the builder AND the reviewer (the
    // validator's author), a third seat, and a turn runs on two ("with one seat, the person edits
    // directly or adds a seat"). The pinned judge is still convened whenever such a seat is free;
    // with none, the gate's receipt records it skipped (`no_distinct_seat`) instead of holding.
    if name == crate::apps::APPS_TURN {
        return Some(vec![crate::assurance::DISTINCT_EVALUATOR.to_string()]);
    }
    // (§9.3) An Apps publish requires QE acceptance, which its launch forces.
    matches!(
        name,
        "feature"
            | "bug"
            | "editor-plugin"
            | "mcp-server"
            | "migration"
            | "apps-publish"
            // (DES-CATALOG-002 §5.1) The catalog's new application-changing presets.
            | "modernize"
            | "refactor"
            | "release"
            | "scaffold"
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

/// (DES-CHAT-FIRST-001 C2) How a built-in preset is described to an operator: a short `title`, a
/// one-line `summary`, and 2–4 `use_when` phrases (when to pick it). Plain operator language — this
/// copy is what a picker shows and what the chat's workflow digest offers. Display data only: it is
/// never part of a composed def (the engine's `WorkflowDef` parser is strict and never sees it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetDescriptor {
    pub title: &'static str,
    pub summary: &'static str,
    pub use_when: &'static [&'static str],
}

/// (DES-CHAT-FIRST-001 C2) The descriptor of built-in preset `name` (`None` for a name that is not a
/// built-in). Code data beside [`builtin_presets`], as [`builtin_preset_instruments`] is: a listing
/// stamps it on the built-in's row when it reads it, so a re-seed never drops it and an existing
/// store gains it with no rewrite. Every built-in has one (pinned by a test).
pub fn builtin_preset_descriptor(name: &str) -> Option<PresetDescriptor> {
    let (title, summary, use_when): (&str, &str, &[&str]) = match name {
        "analyze" => (
            "Analyze code",
            "Answer a question about one or more repositories from the code graph and the files, cited, changing nothing.",
            &[
                "how does this part of the code work",
                "who calls this, what breaks if it changes",
                "a cited answer without changing code",
            ],
        ),
        "apps-publish" => (
            "Publish an app",
            "Check an app against its publish manifest, run QE on a seat that built none of it, then produce it after you consent.",
            &[
                "publish an app from the Apps section",
                "produce the app's site image or embed",
            ],
        ),
        "apps-turn" => (
            "Change an app",
            "Make one change to an app's spec and queries, check it without touching any data, and have a second seat review it.",
            &[
                "a chat message in the Apps builder",
                "change an app's pages, charts or queries",
            ],
        ),
        "bug" => (
            "Fix a bug",
            "Find the cause of a bug, reproduce it, fix it and check that the fix holds.",
            &[
                "something is broken",
                "an error or crash to fix",
                "something that used to work no longer does",
            ],
        ),
        "capture-learnings" => (
            "Capture learnings",
            "Read a repository's recent changes and write down what they teach for future work.",
            &[
                "save lessons after a stretch of work",
                "record what changes often and why",
            ],
        ),
        "chat" => (
            "Chat",
            "Answer questions about your code and work without changing anything.",
            &[
                "ask a question",
                "explain or explore code",
                "talk something through",
            ],
        ),
        "demo" => (
            "Demo video",
            "Plan, record and review a narrated demo video of an app.",
            &["record a product demo", "show a feature working on video"],
        ),
        "demo-human-review" => (
            "Demo video (reviewed by you)",
            "Plan and record a narrated demo video of an app whose data stays out of model context; you review the take.",
            &[
                "record a demo of an app with private data",
                "a demo no model may watch",
            ],
        ),
        "docs" => (
            "Write documentation",
            "Write or update Markdown documentation in the repository from the code, with every claim checked against it.",
            &[
                "write a README, guide or API reference",
                "bring docs back in line with the code",
            ],
        ),
        "domain-extraction" => (
            "Map the domain",
            "Work out what a codebase does as a set of requirements and check how much of the code they cover.",
            &[
                "document what an existing system does",
                "build a requirements map from code",
            ],
        ),
        "editor-plugin" => (
            "Editor plugin",
            "Design, build, test and install a studio editor plugin for a kind of file.",
            &[
                "edit a new kind of file in studio",
                "change how studio edits an artifact",
            ],
        ),
        "feature" => (
            "Build a feature",
            "Clarify, design, build, review and test a change to your application.",
            &[
                "add new behaviour to an app",
                "a new screen, endpoint or option",
                "a change that needs a design and tests",
            ],
        ),
        "interactive-chat" => (
            "Discuss a document",
            "Talk a document through and revise it from the conversation.",
            &[
                "go over a document together",
                "revise a document by discussing it",
            ],
        ),
        "interactive-draft" => (
            "Draft a document",
            "Write a new document from a brief.",
            &[
                "write a new doc, spec or proposal",
                "turn notes into a first draft",
            ],
        ),
        "interactive-edit" => (
            "Edit a document",
            "Make the changes you ask for to an existing document.",
            &[
                "change an existing document",
                "tighten or restructure a draft",
            ],
        ),
        "learn" => (
            "Learn a topic",
            "Research a topic or event from outside sources and save what was learned to the repository.",
            &[
                "learn about a library, tool or standard",
                "catch up on a recent event",
                "refresh knowledge saved earlier",
            ],
        ),
        "mcp-server" => (
            "MCP server",
            "Design, build, test, review and install an MCP server.",
            &[
                "build a new MCP server",
                "give agents a new tool",
            ],
        ),
        "migration" => (
            "Migration",
            "Plan and carry out a move to something new, with a confirmed cutover, then remove the old path.",
            &[
                "move to a new library, service or schema",
                "replace an old system step by step",
                "a change that needs a cutover",
            ],
        ),
        "onboarding" => (
            "Set up a repository",
            "Index a repository so the agents can find their way around it.",
            &["add a new repository", "re-index a repository"],
        ),
        "qe-author-tests" => (
            "Write tests",
            "Write tests for how existing code behaves and run them with the repository's own test setup.",
            &[
                "add missing tests",
                "cover existing behaviour with tests",
            ],
        ),
        "steering-author" => (
            "Write steering rules",
            "Read your sources and propose steering rules for you to approve.",
            &[
                "turn team practices into rules",
                "propose rules from docs or code",
            ],
        ),
        "test-analysis" => (
            "Analyze tests",
            "Find the risky code with no tests, weak or flaky tests and what would close each gap, changing nothing.",
            &[
                "where are we under-tested",
                "audit the quality of a test suite",
                "find flaky or empty tests",
            ],
        ),
        "test-plan" => (
            "Plan tests",
            "Write a risk-ranked test plan for a change or an area of the code, ready to automate.",
            &[
                "decide what to test before building",
                "a test strategy for a feature or release",
            ],
        ),
        "validate-docs" => (
            "Check documentation",
            "Check every claim in your documentation against the code and report what is wrong or stale, changing nothing.",
            &[
                "are the docs still true",
                "find stale commands, flags or examples",
            ],
        ),
        // (DES-CATALOG-002 §3) The catalog-by-verb presets.
        "analyze-codebase" => (
            "Analyze a codebase",
            "Explain how part of the code works, from its entry points to its effects, every claim checked.",
            &[
                "how does this part of the system work",
                "map the flows and modules of a service",
            ],
        ),
        "compliance-review" => (
            "Compliance review",
            "Check the code against a standard or policy, control by control, with cited evidence and gaps.",
            &[
                "are we compliant with this policy",
                "audit our controls against a standard",
            ],
        ),
        "decide" => (
            "Decide (ADR)",
            "Lay out the options for a decision, score them with evidence, and record the option you pick.",
            &[
                "choose between two or more approaches",
                "write up an architecture decision",
            ],
        ),
        "investigate-incident" => (
            "Incident or root cause",
            "Find the root cause of an incident from its error, logs and timeline, cited, changing nothing.",
            &[
                "why did this break in production",
                "find the root cause of an outage",
                "explain this error from its logs",
            ],
        ),
        "map-requirements" => (
            "Map requirements",
            "Trace each requirement to the code and tests that satisfy it, and name what is missing.",
            &[
                "which requirements are implemented",
                "build a traceability matrix",
            ],
        ),
        "modernize" => (
            "Modernize (move)",
            "Move a system to a new stack: discover, blueprint, transform, check parity, cut over and clean up.",
            &[
                "move a service to a new language or stack",
                "replace a legacy component",
            ],
        ),
        "plan" => (
            "Plan it",
            "Turn an ask into a checked plan of jobs, each with its risk and how it will be checked.",
            &[
                "plan a larger piece of work",
                "break an idea into jobs",
            ],
        ),
        "privacy-review" => (
            "Privacy review",
            "Check how the code handles personal data: collection, storage, access, sharing, retention.",
            &[
                "where do we handle personal data",
                "review a change for privacy",
            ],
        ),
        "reconstruct" => (
            "Reconstruct what happened",
            "Build a cited timeline of what changed and how it led to an outcome, changing nothing.",
            &[
                "what changed before this broke",
                "reconstruct the history of a decision",
            ],
        ),
        "refactor" => (
            "Refactor or polish",
            "Restructure or tidy code without changing what it does, with parity checked before and after.",
            &[
                "clean up or restructure code",
                "rename or simplify without behaviour change",
            ],
        ),
        "release" => (
            "Prepare a release",
            "Check release readiness, bump versions and write the changelog in a pull request; never tags.",
            &[
                "prepare the next release",
                "bump versions and write the changelog",
            ],
        ),
        "review-change" => (
            "Review a change",
            "Review a pull request, branch or commit and recommend approve, conditions or reject.",
            &[
                "review this pull request",
                "is this change safe to merge",
            ],
        ),
        "risk-analysis" => (
            "Risk analysis",
            "Assess the risks of a change or area from its blast radius, tests and churn, as a risk matrix.",
            &[
                "what could go wrong with this change",
                "assess the risk of touching this code",
            ],
        ),
        "scaffold" => (
            "Start something new",
            "Shape, design, build and test a new command-line tool, service or library.",
            &[
                "start a new CLI, service or library",
                "scaffold a new project",
            ],
        ),
        "security-review" => (
            "Security review",
            "Review the code for vulnerabilities and weaknesses, each cited with its fix.",
            &[
                "review this code for security issues",
                "check our attack surface",
            ],
        ),
        "suite-health" => (
            "Keep the suite healthy",
            "Find and fix flaky, weak, stale-snapshot or slow tests without touching the code under test.",
            &[
                "fix flaky tests",
                "strengthen weak tests",
                "speed up a slow test suite",
            ],
        ),
        _ => return None,
    };
    Some(PresetDescriptor {
        title,
        summary,
        use_when,
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

/// `analyze` (crew#1157): answer ONE question about the code in scope, read-only, as data
/// (`src/presets/analyze.json`). One `understand` step, `answer`: neutral, `executes_code: false`,
/// no pin, so its write posture is read-only on a bound or unbound run (every write tool and every
/// shell write is refused at the boundary, [`crate::write_posture`]), the floor is empty, there is
/// no creator step (no PA scope, no deliver) and no `qe_acceptance` (it is not in
/// [`builtin_preset_instruments`]). It runs the `wicked-garden-search` skill: grounded in the estate
/// graph through the read-only shim, confirmed by reading the files, every claim cited (file:line,
/// symbol or graph path), and a repository with no bound graph is named as not indexed. The gate is
/// the UNCONDITIONAL turn gate: approve ends the answer, a send-back is the follow-up question.
/// crew's chat runs each turn of an analysis chat as this step (`POST /chats {workflow: "analyze"}`).
fn analyze_preset() -> Vec<PlanStep> {
    let plan: crate::plan::PlanSteps = serde_json::from_str(include_str!("presets/analyze.json"))
        .expect("src/presets/analyze.json is a valid plan");
    plan.steps
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

/// `demo` (M9b; DES-DEMO-RECORD-001, core#1249): plan, record and review a product demo of the app
/// the person names, from crew's demo route (it mints the demo root, writes its launch manifest and
/// hands the root as the run's evidence root; the run's only write root is `<root>/plan`).
///
/// - `probe` → `demo_probe` (engine Tool, on the host, jailed to the app's one port): loads the
///   launch page read-only and writes a page summary the plan seat reads (its container cannot reach
///   the app). Structure only when the app's rows may not reach a model.
/// - `plan` → `produce` (creator), gated `human_confirm`: THE PLAN GATE. The presenter script, the
///   chapter list and the storyline AS DATA (`storyline.json`); the engine snapshots them when the
///   step passes, and the gate shows that snapshot.
/// - `record` → `demo_record` (engine Tool, on the host, jailed to the app's one port): records the
///   APPROVED storyline, stitches the MP4 and writes the contact sheets. No seat runs here; the unit
///   passes only on the engine's receipt of the take (the demo floor). A refusal (the host could not
///   record) offers a retry; a take that failed goes back to `plan`.
/// - `review` → `critique` (evaluator), gated `human_confirm`: THE REVIEW GATE. A seat other than
///   the plan's judges the sheets; `request_changes` goes back to `plan`.
///
/// The recorder is read-only against the app (a request policy in garden's `readonly.mjs`), and
/// synthetic data is labelled.
fn demo_preset() -> Vec<PlanStep> {
    let confirm = Some(GateSpec::HumanConfirm {
        unconditional: false,
    });
    vec![
        demo_probe_step(),
        demo_plan_step(),
        demo_record_step(None),
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

/// `demo-human-review` (DES-DEMO-RECORD-001 §3.7, operator decision 7.1 b): the Demo video of an app
/// whose rows may not reach a model. No model review step: the person reviews the take at the record
/// step's own gate. Crew picks it when the app's sources are not agent-queryable.
fn demo_human_review_preset() -> Vec<PlanStep> {
    vec![
        demo_probe_step(),
        demo_plan_step(),
        demo_record_step(Some(GateSpec::HumanConfirm {
            unconditional: false,
        })),
    ]
}

fn demo_probe_step() -> PlanStep {
    PlanStep {
        catalog: crate::demo_record::PROBE_CATALOG_ID.to_string(),
        id: "probe".to_string(),
        ..PlanStep::default()
    }
}

fn demo_plan_step() -> PlanStep {
    PlanStep {
        catalog: "produce".to_string(),
        id: "plan".to_string(),
        gate: Some(GateSpec::HumanConfirm {
            unconditional: false,
        }),
        skill_ref: Some(DEMO_SKILL.to_string()),
        instructions: Some(DEMO_PLAN_INSTRUCTIONS.to_string()),
        required_deliverables: Some(
            ["script.md", "chapters.json", "storyline.json"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        depends_on: Some(vec!["probe".to_string()]),
        ..PlanStep::default()
    }
}

fn demo_record_step(gate: Option<GateSpec>) -> PlanStep {
    PlanStep {
        catalog: crate::demo_record::RECORD_CATALOG_ID.to_string(),
        id: "record".to_string(),
        gate,
        depends_on: Some(vec!["plan".to_string()]),
        ..PlanStep::default()
    }
}

/// The `plan` step's instructions. Every path is relative to the plan directory the task names.
pub const DEMO_PLAN_INSTRUCTIONS: &str = "Run the wicked-garden-demo skill's `plan` action for \
the app and audience the task names, as its \"In a crew Demo video run\" section says. Your \
container cannot reach the app: read the page summary in the demo root's `probe/` directory (the \
engine loaded the app's launch page for you) and target only what it lists. Write exactly three \
files in the plan directory the task names: `script.md` (the presenter script from the skill's \
template: audience, pitch, run of show, measured vs estimate labelled, which data is synthetic), \
`chapters.json` (a JSON array, one object per chapter: {\"key\": \"NN-slug\", \"title\", \
\"blurb\", \"tags\": [..]}) and `storyline.json` (the storyline AS DATA, the skill's \
`refs/storyline-json.md`: a `ready` target, one segment per chapter with the same keys in the same \
order, steps from its closed list only, every `go` path under the app's launch path, plain text). \
The engine records it on the host after the person approves it; the recording is read-only: show a \
control, never press one that saves, launches, approves or deletes. Write nothing else anywhere.";

/// The `review` step's instructions (an evaluator: it reads and judges, it writes nothing).
pub const DEMO_REVIEW_INSTRUCTIONS: &str = "Run the wicked-garden-demo skill's `review` action \
on the recording in the demo root the task names: look at `review/chapters.png`, \
`review/joins.png` and `review/end.png`, and read `review/ffprobe.json` for the chapters, size \
and codec (the engine probed `demo-video/demo.mp4` for you). You judge work the engine recorded \
from another seat's plan; change no file. Check every caption against its picture, the joins, the \
held closing card, and that synthetic data is labelled. Report every finding as `timestamp · \
chapter · what's wrong · verdict` (verdict: re-encode, re-record, or fix-app), then end with one \
fenced ```json block: {\"verdict\": \"accept\" or \"changes\", \"findings\": [{\"at\": \
\"m:ss\", \"chapter\": \"<key>\", \"issue\": \"..\", \"verdict\": \
\"re-encode|re-record|fix-app\"}]}.";

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
    // (DES-TEAMING-002 §8.8, operator decision 2026-10-18) triage is a second view the PA's fix
    // rests on: a member seat does it and the PA accepts or rejects the result at its next
    // boundary. On a roster with no seat but the PA's it runs on the PA, disclosed
    // (`distribute::distribute_seated`). reproduce is a Test-stage entry: the evaluator fence
    // already seats it off the builders, and on the PA (a rejected member step re-planned there)
    // the gate would deny it as the author's own test — so it stays the PA's step.
    vec![
        PlanStep {
            owner: Some(StepOwner::Team),
            ..step("understand", "triage", None)
        },
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
    // (DES-TEAMING-002 §8.8, operator decision 2026-10-18) churn and hotspots read the repo and
    // change nothing: a member seat does them, the PA reviews them and writes the capture.
    vec![
        PlanStep {
            owner: Some(StepOwner::Team),
            ..step("understand", "churn", CAPTURE_CHURN_INSTRUCTIONS, None)
        },
        PlanStep {
            owner: Some(StepOwner::Team),
            ..step(
                "understand",
                "hotspots",
                CAPTURE_HOTSPOTS_INSTRUCTIONS,
                Some("churn"),
            )
        },
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
        // (DES-CATALOG-002 §5.1) verify checks parity, and parity is checked again on the final
        // tree, after cleanup (the freshness rule would add it; the preset names it).
        PlanStep {
            rubric: Some(crate::rubric::PARITY.to_string()),
            ..step("test", "verify", Some("cutover"))
        },
        PlanStep {
            gate_type: Some(None),
            ..step("build", "cleanup", Some("verify"))
        },
        PlanStep {
            rubric: Some(crate::rubric::PARITY.to_string()),
            ..step("test", "parity", Some("cleanup"))
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

/// The Apps presets (DES-APPS-001 §9.2-§9.3, K0) as data (`src/presets/apps-turn.json`,
/// `src/presets/apps-publish.json`). turn: build → `build` (the creator, skill `wicked-garden-apps`,
/// `apps.json` and `queries/` only); check → `run`, crew's fixed `wicked-crew apps-check` (a Tool:
/// no agent code runs); review → `review` (a distinct evaluator, the evidence floor and the judge).
/// publish: check → `run` (`wicked-crew apps-check --publish`); qe → `critique` (the run's QE unit,
/// [`crate::plan::plan_from_def`]; no diff floor, since a publish changes no tree); produce → `run`
/// (`wicked-crew apps-produce`) behind `consent_before`, depending on the check alone: its consent
/// card is read from the trusted check's output, never from the QE seat's. They launch as FIXED plans
/// ([`crate::apps`]): never scoped, floor-filled or re-planned by the plan gate.
/// (DES-CATALOG-002 §3.2) The catalog-by-verb presets as data (`src/presets/<name>.json`): each a
/// plan over existing catalog entries and garden skills, judged by a rubric ([`crate::rubric`]) on
/// its check step. The read-only ones (Investigate, Analyze, Judge: `investigate-incident`,
/// `reconstruct`, `analyze-codebase`, `map-requirements`, `review-change`, `risk-analysis`,
/// `security-review`, `privacy-review`, `compliance-review`, `decide`) and `plan` (its plan is the
/// reply) have no creator, no Tool and no code step, so they never write and never deliver;
/// `refactor`, `release`, `scaffold`, `modernize` and `suite-health` change the repo.
fn catalog_preset(json: &str, name: &str) -> Vec<PlanStep> {
    let plan: crate::plan::PlanSteps = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("src/presets/{name}.json is a valid plan: {e}"));
    plan.steps
}

fn apps_preset(json: &str, name: &str) -> Vec<PlanStep> {
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

/// `learn` (DES-learn-workflow, lane w9-learn 2026-10-11): learn a topic or an event from outside
/// sources into the repository as KNOWLEDGE, as data (`src/presets/learn.json`). scope, research,
/// synthesize → `understand` (neutral: they write nothing; their output is the record the next step
/// reads); walkthrough → `understand` behind an UNCONDITIONAL human gate (the chat walkthrough: the
/// operator approves or sends it back with questions, and the engagement dial can never skip it);
/// author → `build` (the creator, evidence floor, `executes_code`, so a repo-scoped launch delivers a
/// pull request by default) writing `.wicked/knowledge/<slug>/` and text-only skills; review →
/// `review` on a distinct evaluator seat. Every step runs `wicked-garden-learn`. No `qe_acceptance`:
/// the run changes knowledge files and text-only skills, not the application (the skill's check
/// refuses an executable in a learn skill). Indexing into estate happens outside the run, from merged
/// content (a run's estate is read-only).
fn learn_preset() -> Vec<PlanStep> {
    let plan: crate::plan::PlanSteps = serde_json::from_str(include_str!("presets/learn.json"))
        .expect("src/presets/learn.json is a valid plan");
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
/// extract-prepare → `run` (the host's Tool step: the hub-first worklist, wicked-garden#1362);
/// extract → `produce` (the bold cell: kind recon → build; the seat states rules into a file);
/// critique → `critique` (the page's review: the stated rules, not the repository's completeness);
/// extract-apply → `run` (the host's Tool step: checks the rules file and writes the store);
/// extract-next → `run` (the host's Tool step that loops back to extract-prepare for the next page
/// until the target or the budget's ask, `repeat_from`);
/// coverage → `domain_coverage`, which
/// carries `COVERAGE_VALIDATOR_PIN` as data (the step restates it, a no-op), with `executes_code`
/// raised and its report as the deliverable; domain-graph → `run`, the Tool step that builds the
/// requirements graph, behind a `human_confirm` gate. The skills are the domain family's.
fn domain_extraction_preset() -> Vec<PlanStep> {
    use crate::domain_extraction::{
        garden_domain_step_cmd, COVERAGE_REPORT_FILE, EXTRACT_APPLY_STEP, EXTRACT_NEXT_STEP,
        EXTRACT_PREPARE_STEP, RULES_REL, RULES_REVIEW_INSTRUCTIONS, WORKLIST_REL,
    };
    use crate::workflow::{CODE_GRAPH_DB_TOKEN, REPO_ROOT_TOKEN};
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
        // wicked-garden#1362: the seat cannot reach a store or the engine's binaries, so the host
        // prepares its work list and applies what it states — files in the worktree between them.
        PlanStep {
            gate_type: Some(None),
            executor: Some(PhaseExecutor::Tool {
                cmd: garden_domain_step_cmd(
                    EXTRACT_PREPARE_STEP,
                    "scripts/domain/prepare_worklist.py",
                    &format!("--out {WORKLIST_REL}"),
                ),
            }),
            ..step("run", EXTRACT_PREPARE_STEP, Some("analyze"))
        },
        PlanStep {
            skill_ref: Some("wicked-garden-domain-extractor".to_string()),
            ..step("produce", "extract", Some(EXTRACT_PREPARE_STEP))
        },
        // The page's own review (a `critique`, so the floor adds no generic one that judges a page
        // as an incomplete model): before the host writes anything.
        PlanStep {
            instructions: Some(RULES_REVIEW_INSTRUCTIONS.to_string()),
            ..step("critique", "critique", Some("extract"))
        },
        PlanStep {
            gate_type: Some(None),
            executor: Some(PhaseExecutor::Tool {
                cmd: garden_domain_step_cmd(
                    EXTRACT_APPLY_STEP,
                    "scripts/domain/apply_rules.py",
                    &format!("--rules {RULES_REL} --coverage-out {COVERAGE_REPORT_FILE}"),
                ),
            }),
            ..step("run", EXTRACT_APPLY_STEP, Some("critique"))
        },
        // domain-graph-3: the generation keeps going until the repository's target or the
        // operator stops it — this host step decides each page's next (another page from the
        // frontier, the budget's ask, or done), a bounded loop back to `extract-prepare`.
        PlanStep {
            gate_type: Some(None),
            executor: Some(PhaseExecutor::Tool {
                cmd: garden_domain_step_cmd(EXTRACT_NEXT_STEP, "scripts/domain/next_page.py", ""),
            }),
            repeat_from: Some(EXTRACT_PREPARE_STEP.to_string()),
            ..step("run", EXTRACT_NEXT_STEP, Some(EXTRACT_APPLY_STEP))
        },
        PlanStep {
            executes_code: Some(true),
            required_deliverables: Some(vec!["coverage-report.json".to_string()]),
            skill_ref: Some(DOMAIN_COVERAGE_SKILL.to_string()),
            validator_pin: Some(Some(COVERAGE_VALIDATOR_PIN.to_string())),
            ..step("domain_coverage", "coverage", Some(EXTRACT_NEXT_STEP))
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
                    // The graph is saved into the repo's estate store (`--db`), where crew reads
                    // it (`requirements_graph_for_repo`); no file is written into the checkout
                    // (operator ruling 2026-10-31). `--repo-root` names the checkout whose
                    // tracked files the corroboration reads. The coverage minimum reaches the
                    // step through the daemon's environment
                    // (`WICKED_REQUIREMENTS_MIN_COVERAGE_PCT`, crew's setting).
                    "--repo-root",
                    REPO_ROOT_TOKEN,
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
        PhaseDef {
            // (DES-DEMO-RECORD-001) The read-only page probe the demo plan writes from: the engine's
            // own fixed command, jailed to the app's one port (`demo_record` module).
            executor: PhaseExecutor::Tool {
                cmd: crate::demo_record::DEMO_PROBE_CMD
                    .iter()
                    .map(|a| a.to_string())
                    .collect(),
            },
            ..entry(
                crate::demo_record::PROBE_CATALOG_ID,
                Recon,
                Neutral,
                auto,
                Value,
                None,
                false,
            )
        },
        PhaseDef {
            // (DES-DEMO-RECORD-001) The recorder: the engine's own fixed command on the host, jailed
            // to the app's one port, recording the APPROVED storyline; its unit passes only on the
            // engine's receipt of the take (the demo floor), so it carries no pin.
            executor: PhaseExecutor::Tool {
                cmd: crate::demo_record::DEMO_RECORD_CMD
                    .iter()
                    .map(|a| a.to_string())
                    .collect(),
            },
            ..entry(
                crate::demo_record::RECORD_CATALOG_ID,
                Build,
                Neutral,
                auto,
                Value,
                None,
                false,
            )
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
        answer_only: false,
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
        rubric: None,
        repeat_from: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §8.3 table, cell by cell, as fixed values (not re-derived from the builder).
    #[test]
    fn the_catalog_is_the_seventeen_entries_of_the_table() {
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
            ("domain_coverage", j("test"), j("evaluator"), hci.clone(), j("execution"), Some("9fddcc159580ed33"), false, j("agent"), None),
            ("run", j("recon"), j("neutral"), j("auto"), j("value"), None, false, j("tool"), None),
            ("deliver", j("build"), j("neutral"), j("auto"), j("execution"), None, false, j("tool"), None),
            ("demo_probe", j("recon"), j("neutral"), j("auto"), j("value"), None, false, j("tool"), None),
            ("demo_record", j("build"), j("neutral"), j("auto"), j("value"), None, false, j("tool"), None),
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

    /// wicked-garden#1362: a seat container has no `wicked-core`, `wicked-estate` or writable
    /// store, so the host brackets the seat's `extract`: `extract-prepare` writes the worklist
    /// before it, `extract-apply` checks and writes its rules after it, and coverage waits for the
    /// apply. Both are Tool steps (the engine's own command on the host, no seat) that run garden's
    /// scripts through the admitted generation's launcher — never a PATH `wicked-garden` — with the
    /// store and repository bound by placeholder, whole arguments only.
    #[test]
    fn domain_extraction_brackets_the_seat_with_host_steps() {
        use crate::domain_extraction::{
            EXTRACT_APPLY_STEP, EXTRACT_NEXT_STEP, EXTRACT_PREPARE_STEP,
        };
        let def = composed("domain-extraction");
        assert_eq!(
            def.phases.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            [
                "survey",
                "analyze",
                EXTRACT_PREPARE_STEP,
                "extract",
                "critique",
                EXTRACT_APPLY_STEP,
                EXTRACT_NEXT_STEP,
                "coverage",
                "domain-graph"
            ]
        );
        let by = |id: &str| def.phases.iter().find(|p| p.id == id).unwrap();
        assert_eq!(by("extract").depends_on, [EXTRACT_PREPARE_STEP]);
        // The page's review sits between the seat and the host's write, judging stated rules
        // (rig run 7588cff1: a generic critique failed a page of wicked-crew as incomplete).
        let review = by("critique");
        assert_eq!(review.role, PhaseRole::Evaluator);
        assert_eq!(review.depends_on, ["extract"]);
        assert!(review
            .instructions
            .as_deref()
            .is_some_and(|i| i.contains("one page at a time")
                && i.contains("Completeness is not your question")));
        assert_eq!(by(EXTRACT_APPLY_STEP).depends_on, ["critique"]);
        // domain-graph-3: after each page the host decides the next — the loop back to
        // extract-prepare (the generation keeps going until the target or the operator's word).
        assert_eq!(by(EXTRACT_NEXT_STEP).depends_on, [EXTRACT_APPLY_STEP]);
        assert_eq!(
            by(EXTRACT_NEXT_STEP).repeat_from.as_deref(),
            Some(EXTRACT_PREPARE_STEP)
        );
        assert!(
            def.phases
                .iter()
                .filter(|p| p.repeat_from.is_some())
                .count()
                == 1
        );
        assert_eq!(by("coverage").depends_on, [EXTRACT_NEXT_STEP]);
        for (id, script) in [
            (EXTRACT_PREPARE_STEP, "scripts/domain/prepare_worklist.py"),
            (EXTRACT_APPLY_STEP, "scripts/domain/apply_rules.py"),
            (EXTRACT_NEXT_STEP, "scripts/domain/next_page.py"),
        ] {
            let phase = by(id);
            assert_eq!(phase.role, PhaseRole::Neutral, "{id}");
            assert_eq!(phase.gate, GateSpec::Auto, "{id}");
            let PhaseExecutor::Tool { cmd } = &phase.executor else {
                panic!("{id} is not a Tool step");
            };
            assert_eq!(&cmd[..2], ["bash", "-c"], "{id}");
            assert!(
                cmd[2].starts_with("exec \"${WICKED_GARDEN_ROOT:?")
                    && cmd[2].contains(&format!("/scripts/wicked-garden\" run {script} "))
                    && cmd[2].contains("--db \"$1\" --repo-root \"$2\""),
                "{id}: {}",
                cmd[2]
            );
            assert_eq!(
                &cmd[3..],
                [
                    id,
                    crate::workflow::CODE_GRAPH_DB_TOKEN,
                    crate::workflow::REPO_ROOT_TOKEN
                ],
                "{id}"
            );
        }
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

    /// DES-CHAT-FIRST-001 C2: every built-in preset has a title, a one-line summary and 2–4 short
    /// `use_when` phrases, and no name outside the built-ins has a descriptor.
    #[test]
    fn every_builtin_preset_has_a_descriptor() {
        for (name, _) in builtin_presets() {
            let d = builtin_preset_descriptor(name)
                .unwrap_or_else(|| panic!("built-in preset `{name}` has no descriptor"));
            assert!(!d.title.trim().is_empty(), "{name}: empty title");
            assert!(d.title.len() <= 40, "{name}: title too long");
            assert!(!d.summary.trim().is_empty(), "{name}: empty summary");
            assert!(!d.summary.contains('\n'), "{name}: summary is one line");
            assert!(d.summary.len() <= 120, "{name}: summary too long");
            assert!(
                (2..=4).contains(&d.use_when.len()),
                "{name}: {} use_when phrases",
                d.use_when.len()
            );
            for phrase in d.use_when {
                assert!(
                    !phrase.trim().is_empty() && phrase.len() <= 60,
                    "{name}: `{phrase}`"
                );
            }
        }
        assert_eq!(builtin_preset_descriptor("no-such-preset"), None);
        assert_eq!(builtin_preset_descriptor("campaign-x-y"), None);
    }

    /// (DES-TEAMING-002 §8.8, operator decision 2026-10-18) The built-in presets hand their
    /// independent, read-only steps to the team: exactly these, each a neutral entry that writes
    /// nothing (never a creator, never an evaluator — the fence seats those), so one accountable
    /// PA still builds and a member's work never changes the tree.
    #[test]
    fn the_builtin_presets_give_their_independent_read_only_steps_to_the_team() {
        let mut team: Vec<(String, String)> = Vec::new();
        for (name, steps) in builtin_presets() {
            for s in steps {
                if s.owner == Some(StepOwner::Team) {
                    let entry = catalog_entry(&s.catalog).expect("a catalog entry");
                    assert_eq!(entry.role, PhaseRole::Neutral, "{name}/{}", s.id);
                    // A Test-stage step re-planned onto the PA would be graded as the author's
                    // own test: only recon steps go to the team.
                    assert_eq!(entry.kind, StageKind::Recon, "{name}/{}", s.id);
                    assert!(!entry.executes_code, "{name}/{}", s.id);
                    assert_ne!(s.executes_code, Some(true), "{name}/{}", s.id);
                    team.push((name.to_string(), s.id.clone()));
                }
            }
        }
        let want: Vec<(String, String)> = [
            ("bug", "triage"),
            ("capture-learnings", "churn"),
            ("capture-learnings", "hotspots"),
            // (DES-CATALOG-002 §6) Investigate's research step is a team step at every depth.
            ("investigate-incident", "trace"),
            ("learn", "research"),
            ("mcp-server", "source-discovery"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        assert_eq!(team, want);
    }

    /// crew#1157: `analyze` can never write. Its one step composes to a neutral, non-code,
    /// unpinned `understand` unit whose write posture is READ-ONLY on a bound and an unbound run,
    /// so the boundary refuses every write tool and every shell write into the tree; it has no
    /// creator (no PA scope, no deliver) and needs no QE acceptance. Its instructions demand the
    /// grounding (the read-only estate shim), the reading that confirms it, the citations, and the
    /// not-indexed line with the "Index this repository" offer.
    #[test]
    fn analyze_is_one_read_only_cited_answer_step_that_can_never_write() {
        use crate::write_posture::WritePosture;
        let steps = builtin_preset("analyze").expect("analyze is a built-in");
        assert_eq!(steps.len(), 1);
        let step = &steps[0];
        assert_eq!(
            (step.catalog.as_str(), step.id.as_str()),
            ("understand", "answer")
        );
        assert_eq!(step.skill_ref.as_deref(), Some("wicked-garden-search"));
        assert_eq!(
            step.gate,
            Some(GateSpec::HumanConfirm {
                unconditional: true
            }),
            "the turn gate: approve ends it, a send-back is the follow-up"
        );
        assert!(step.executor.is_none() && step.executes_code.is_none());
        assert!(step.required_deliverables.is_none() && step.validator_pin.is_none());
        let plan = crate::plan::PlanSteps {
            steps: steps.clone(),
            ..crate::plan::PlanSteps::default()
        };
        assert!(!plan.has_creator(), "no creator: no scope step, no deliver");
        assert_eq!(
            builtin_preset_instruments("analyze"),
            None,
            "no QE acceptance"
        );

        let def = crate::plan::compose(catalog(), &plan).expect("analyze composes");
        assert_eq!(def.phases.len(), 1);
        assert!(
            def.phases.iter().all(|p| p.validator_pin.is_none()),
            "no floor"
        );
        let units = crate::plan::plan_from_def(&def, "How does sign-in work?", "ra");
        assert_eq!(units.len(), 1);
        let u = &units[0];
        assert!(!u.executes_code && u.tool_cmd.is_none());
        assert_eq!(u.role, PhaseRole::Neutral);
        assert!(u.worktree_guarded, "the worktree guard restores any change");
        for bound in [true, false] {
            assert_eq!(
                WritePosture::of(u, bound),
                WritePosture::ReadOnly,
                "bound={bound}"
            );
        }

        let posture = WritePosture::of(u, true);
        let wt = std::path::Path::new("/wt");
        for tool in ["Write", "Edit", "NotebookEdit"] {
            let ctx =
                serde_json::json!({ "path": "/wt/src/app.ts", "file_path": "/wt/src/app.ts" });
            assert!(
                crate::gate_hook::phase_scope_denial(false, posture, &ctx, tool, wt, None, &[])
                    .is_some(),
                "{tool} is refused"
            );
        }
        for cmd in [
            "echo x > /wt/src/app.ts",
            "mkdir -p /wt/evidence",
            "cat > /wt/a.md <<'EOF'\nx\nEOF",
        ] {
            let ctx = serde_json::json!({ "command": cmd });
            assert!(
                crate::gate_hook::phase_scope_denial(false, posture, &ctx, "Bash", wt, None, &[])
                    .is_some(),
                "a shell write is refused: {cmd}"
            );
        }
        let read = serde_json::json!({ "command": "sed -n 1,40p /wt/src/app.ts" });
        assert_eq!(
            crate::gate_hook::phase_scope_denial(false, posture, &read, "Bash", wt, None, &[]),
            None,
            "reads are allowed"
        );

        // xreview on #1136 (codex P2): with the roots dispatch actually arms. A bound unit gets a
        // notes root (minted outside the tree, core#464 / R7) and it is the ONLY admitted root:
        // every write to the tree is still refused with it armed, and a write under it never
        // touches a repository.
        let base =
            std::env::temp_dir().join(format!("wicked-analyze-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (tree, notes) = (base.join("wt"), base.join("notes").join("answer"));
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::create_dir_all(&notes).unwrap();
        let mut dispatched = u.clone();
        dispatched.notes_root = Some(notes.to_string_lossy().into_owned());
        let roots = crate::write_posture::unit_admitted_roots(posture, &dispatched, &[]);
        assert_eq!(
            roots,
            vec![notes.clone()],
            "the notes root is the only admitted root"
        );
        assert!(
            !notes.starts_with(&tree),
            "the notes root is outside the tree"
        );
        let w = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let in_tree = w(&tree.join("src").join("app.ts"));
        for tool in ["Write", "Edit", "NotebookEdit"] {
            let ctx = serde_json::json!({ "path": in_tree, "file_path": in_tree });
            assert!(
                crate::gate_hook::phase_scope_denial(
                    false, posture, &ctx, tool, &tree, None, &roots
                )
                .is_some(),
                "{tool} into the tree is refused with the notes root armed"
            );
        }
        for cmd in [
            format!("echo x > {in_tree}"),
            format!("mkdir -p {}", w(&tree.join("evidence"))),
        ] {
            let ctx = serde_json::json!({ "command": cmd });
            assert!(
                crate::gate_hook::phase_scope_denial(
                    false, posture, &ctx, "Bash", &tree, None, &roots
                )
                .is_some(),
                "a shell write into the tree is refused with the notes root armed: {cmd}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);

        let text = step.instructions.as_deref().unwrap();
        for must in [
            "READ-ONLY",
            "_estate_client.py --readonly call",
            "SearchEntity",
            "BlastRadius",
            "Lineage",
            "RankHotspots",
            "knowledge.recall",
            "read every file you cite",
            "`alpha/src/foo.ts:42`",
            "Every claim cites the file and line it rests on",
            "never in place of the file and line",
            "graph path",
            "is not indexed",
            "Index this repository",
            "only when no code graph is bound",
            "finds nothing for a symbol",
            "keep using the graph",
        ] {
            assert!(text.contains(must), "analyze instructions name `{must}`");
        }
        // The instructions never promise more than the boundary enforces (xreview on #1136).
        assert!(!text.contains("every write is refused"), "{text}");
        assert!(
            text.contains("every write to the code is refused"),
            "{text}"
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

    /// (DES-DEMO-RECORD-001) No seat records a demo: both demo presets record with the engine's
    /// `demo_record` Tool after the plan gate, the plan writes `storyline.json` (data, never a
    /// module), and only the model-reviewed preset carries a review step.
    #[test]
    fn the_demo_presets_record_on_the_host_from_data() {
        for (name, want_review) in [("demo", true), ("demo-human-review", false)] {
            let steps = builtin_presets()
                .into_iter()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| s)
                .unwrap_or_else(|| panic!("{name} is a builtin preset"));
            let ids: Vec<_> = steps
                .iter()
                .map(|s| (s.id.as_str(), s.catalog.as_str()))
                .collect();
            assert_eq!(
                &ids[..3],
                [
                    ("probe", "demo_probe"),
                    ("plan", "produce"),
                    ("record", "demo_record")
                ],
                "{name}"
            );
            assert_eq!(ids.len() == 4, want_review, "{name}: {ids:?}");
            let plan = &steps[1];
            assert!(plan
                .required_deliverables
                .as_ref()
                .unwrap()
                .contains(&"storyline.json".to_string()));
            assert!(
                steps[2].required_deliverables.is_none(),
                "the receipt is the record step's floor"
            );
            assert_eq!(
                steps[2].gate.is_some(),
                !want_review,
                "{name}: the person reviews at record's gate"
            );
        }
        assert!(
            DEMO_PLAN_INSTRUCTIONS.contains("storyline.json")
                && !DEMO_PLAN_INSTRUCTIONS.contains("storyline.mjs")
        );
        assert!(DEMO_REVIEW_INSTRUCTIONS.contains("ffprobe.json"));
    }

    /// (DES-CATALOG-002 §3.1) The read-only verbs: every Investigate, Analyze and Judge preset —
    /// and Plan it, whose plan is its reply (a report, never a change).
    const READ_ONLY_PRESETS: [&str; 12] = [
        "plan",
        "investigate-incident",
        "reconstruct",
        "analyze-codebase",
        "map-requirements",
        "validate-docs",
        "review-change",
        "risk-analysis",
        "security-review",
        "privacy-review",
        "compliance-review",
        "decide",
    ];

    /// (DES-CATALOG-002 §11 Lane A) Every Investigate, Analyze and Judge preset composes with no
    /// deliver step and no step that may write: no creator, no Tool, nothing that executes code —
    /// and, with no creator, floor fill adds nothing (no deliver either) even at the top band on a
    /// delivering run. Each ends in a cited-findings critique held for a person when it fails. The
    /// one exception is Domain (`domain-extraction`), whose writes are bounded to `.wicked/domain/`
    /// and the project graph and which never delivers (Q17; `tests/domain_extraction_e2e.rs`
    /// asserts the run's diff).
    #[test]
    fn every_investigate_analyze_and_judge_preset_is_read_only_and_never_delivers() {
        use crate::workflow::{GateCond, GateSpec};
        for name in READ_ONLY_PRESETS {
            let steps = builtin_preset(name).unwrap_or_else(|| panic!("{name} is a built-in"));
            let plan = crate::plan::PlanSteps {
                steps: steps.clone(),
                ..Default::default()
            };
            assert!(!plan.has_creator(), "{name} has a creator");
            for s in &steps {
                assert!(
                    !matches!(s.catalog.as_str(), "build" | "produce" | "run" | "deliver"),
                    "{name}/{}: {}",
                    s.id,
                    s.catalog
                );
                assert_ne!(s.executes_code, Some(true), "{name}/{}", s.id);
            }
            let def = composed(name);
            for p in &def.phases {
                assert!(!p.executes_code, "{name}/{} executes code", p.id);
                assert!(!is_tool_entry(p), "{name}/{} is a Tool", p.id);
                assert_ne!(p.role, PhaseRole::Creator, "{name}/{}", p.id);
            }
            let last = def.phases.last().unwrap();
            assert_eq!(last.catalog.as_deref(), Some("critique"), "{name}");
            assert_eq!(
                last.rubric.as_deref(),
                Some(crate::rubric::CITED_FINDINGS),
                "{name}"
            );
            // validate-docs keeps its own (no gate on `verify`, §5.2); every new one holds a FAIL.
            let gate = if name == "validate-docs" {
                GateSpec::Auto
            } else {
                GateSpec::HumanConfirmIf(GateCond::VerdictNotPass)
            };
            assert_eq!(last.gate, gate, "{name}");
            let deliver = ["wicked-crew".to_string(), "deliver".to_string()];
            let filled = crate::plan::floor_fill(
                catalog(),
                &plan,
                crate::plan::FloorInput {
                    score: 100,
                    destructive: true,
                    human_confirm: &crate::domain::HumanConfirm::None,
                    deliver: Some(&deliver),
                    obligations: &[],
                    ran: &[],
                    requires_evaluator: true,
                    requires_qe: true,
                    power_absorbs: &[],
                },
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                filled.steps.steps.len(),
                steps.len(),
                "{name}: the floor adds nothing"
            );
            assert!(filled.floor.is_empty(), "{name}");
            // Never QE acceptance: a read-only run changes no application.
            assert_eq!(builtin_preset_instruments(name), None, "{name}");
        }
        // The bounded exception: Domain's one creator is its `produce` extract step.
        let domain = builtin_preset("domain-extraction").unwrap();
        let creators: Vec<&str> = domain
            .iter()
            .filter(|s| catalog_entry(&s.catalog).is_some_and(|e| e.role == PhaseRole::Creator))
            .map(|s| s.catalog.as_str())
            .collect();
        assert_eq!(creators, ["produce"]);
        assert!(!domain
            .iter()
            .any(|s| s.catalog == "deliver" || s.catalog == "build"));
    }

    /// (DES-CATALOG-002 §5.1) QE acceptance is required by exactly the application-changing
    /// presets: today's five, the Apps publish, and the catalog's refactor, release, scaffold and
    /// modernize. Every other built-in runs on the defaults.
    #[test]
    fn qe_acceptance_rides_exactly_the_application_changing_presets() {
        let mut qe: Vec<&str> = builtin_presets()
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| {
                builtin_preset_instruments(n)
                    .is_some_and(|i| i.iter().any(|x| x == crate::assurance::QE_ACCEPTANCE))
            })
            .collect();
        qe.sort_unstable();
        assert_eq!(
            qe,
            [
                "apps-publish",
                "bug",
                "editor-plugin",
                "feature",
                "mcp-server",
                "migration",
                "modernize",
                "refactor",
                "release",
                "scaffold"
            ]
        );
    }

    /// (DES-CATALOG-002 §3.2, §5) Each new preset names the rubric its card's gate shape needs, on
    /// the step that judges, and every built-in composes (an unknown or misplaced rubric would
    /// refuse it).
    #[test]
    fn the_new_presets_carry_their_gate_rubrics_on_their_judging_steps() {
        let rubrics = |name: &str| -> Vec<(String, String)> {
            builtin_preset(name)
                .unwrap()
                .into_iter()
                .filter_map(|s| s.rubric.map(|r| (s.id, r)))
                .collect()
        };
        let p = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        assert_eq!(rubrics("plan"), p(&[("check", "cited-findings")]));
        assert_eq!(rubrics("refactor"), p(&[("parity", "parity")]));
        assert_eq!(rubrics("suite-health"), p(&[("review", "cited-findings")]));
        assert_eq!(rubrics("release"), p(&[("fidelity", "fidelity-to-source")]));
        assert_eq!(
            rubrics("modernize"),
            p(&[("parity", "parity"), ("final-parity", "parity")])
        );
        assert_eq!(
            rubrics("migration"),
            p(&[("verify", "parity"), ("parity", "parity")])
        );
        assert_eq!(
            rubrics("review-change"),
            p(&[("review", "cited-findings"), ("semantic", "cited-findings")])
        );
        assert_eq!(rubrics("test-plan"), p(&[("check", "cited-findings")]));
        assert_eq!(rubrics("docs"), p(&[("check", "fidelity-to-source")]));
        assert!(rubrics("scaffold").is_empty());
        // J4/J6: feature, bug, mcp-server and editor-plugin carry no rubric.
        for name in ["feature", "bug", "mcp-server", "editor-plugin"] {
            assert!(rubrics(name).is_empty(), "{name}");
        }
        for (name, _) in builtin_presets() {
            if name == crate::apps::APPS_PUBLISH || name == crate::apps::APPS_TURN {
                continue; // fixed plans, composed by `apps`
            }
            let def = composed(name);
            for ph in def.phases.iter().filter(|ph| ph.rubric.is_some()) {
                let r = crate::rubric::rubric(ph.rubric.as_deref().unwrap()).unwrap();
                assert!(
                    ph.instructions
                        .as_deref()
                        .is_some_and(|i| i.ends_with(&r.instructions)),
                    "{name}/{}: the rubric's contract follows the step's own",
                    ph.id
                );
            }
        }
    }

    /// (DES-CATALOG-002 §5.2, semantic regression) `validate-docs`, `test-analysis` and `docs` keep
    /// their own criteria, their gates and their read-only posture: only the shared cite and VERDICT
    /// sentences moved into the rubric, and the composed step still says every one of them.
    #[test]
    fn validate_docs_test_analysis_and_docs_keep_their_criteria_gates_and_posture() {
        use crate::workflow::{GateCond, GateSpec};
        let cite = "Every claim cites its file and line relative to the repository root";
        let verdict =
            "End with one plain-text line VERDICT: PASS or VERDICT: FAIL as the last line";
        let cases: [(&str, &str, &[&str], GateSpec); 3] = [
            (
                "validate-docs",
                "verify",
                &[
                    "Classify each claim: holds, wrong",
                    "Close with counts per class.",
                    "PASS only when every checkable claim holds; a wrong, stale or unverifiable \
                     claim is a FAIL.",
                    "READ-ONLY",
                ],
                GateSpec::Auto,
            ),
            (
                "test-analysis",
                "verify",
                &[
                    "re-check each finding yourself at file:line and drop any that does not hold",
                    "the one test (or test change) that would close it",
                    "PASS when the report holds no finding you could not confirm.",
                    "READ-ONLY",
                ],
                GateSpec::Auto,
            ),
            (
                "docs",
                "check",
                &[
                    "List EVERY change the run made",
                    "FAIL on any other change",
                    "PASS only when every checkable claim holds and only allowed outputs changed; \
                     an unverifiable claim is a FAIL",
                    "Change nothing yourself",
                ],
                GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
            ),
        ];
        for (name, id, criteria, gate) in cases {
            let def = composed(name);
            let ph = def.phases.iter().find(|p| p.id == id).unwrap();
            let text = ph.instructions.as_deref().unwrap();
            for c in criteria.iter().chain([&cite, &verdict]) {
                assert!(text.contains(c), "{name}/{id} lost `{c}`: {text}");
            }
            assert_eq!(ph.gate, gate, "{name}/{id}: the gate is the preset's own");
            assert!(!ph.executes_code, "{name}/{id}");
            assert_eq!(ph.role, PhaseRole::Evaluator, "{name}/{id}");
            assert!(!text.contains('\n'), "{name}/{id}: one line");
        }
        // The two read-only presets stay creator-free.
        for name in ["validate-docs", "test-analysis"] {
            assert!(composed(name)
                .phases
                .iter()
                .all(|p| p.role != PhaseRole::Creator && !p.executes_code));
        }
    }
}
