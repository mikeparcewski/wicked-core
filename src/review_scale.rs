//! How much review a unit's change summons (S4 of #590): impact scoring.
//!
//! A per-phase council costs six CLI subprocesses whether or not anything interesting happened.
//! Teaming (#590) replaces it with monitors whose number and depth scale with the change. This
//! module is that scaling, and nothing else. It scores a change by what DEPENDS on what it
//! touched, read from the run's repo estate graph, not by how many lines it has: a ten-line edit
//! to a symbol with forty callers outranks a six-hundred-line new leaf file. Nothing here
//! dispatches a monitor; S2 (monitors) and S5 (teamed mode) call [`assess`].
//!
//! The pipeline, each step plain data in and out:
//!
//! 1. [`signals_from_diff`] (pure): unified diff -> [`ChangeSignals`]: the touched non-docs files
//!    with the base-side lines each hunk touches, plus the destructive and critical-path markers.
//!    Diff-only, so destructive detection works without a graph.
//! 2. [`graph_age`] (one `repo_info` read): is the graph indexed AT the run's base commit, the
//!    commit the diff's old side is taken from? Anything else is [`GraphAge::Stale`] or
//!    [`GraphAge::Missing`], and [`assess`] applies this itself.
//! 3. [`impact_signals`] (graph reads): C = the changed symbols, R = their dependents within
//!    `hops` (callers, importers, injected-edge consumers), the products R lands in, whether a
//!    published surface changed, and the test gap.
//! 4. [`impact_score`] (pure): a deterministic 0-100 from the table.
//! 5. An optional [`ModelAssessment`] hook may ADD 0, 10 or 20 with a recorded rationale. It can
//!    never subtract, and it is consulted only once the deterministic score is at least
//!    `model_hook_min_score`. Nothing wires it to a CLI here.
//! 6. [`plan_for`] (pure): score band -> [`ReviewPlan`].
//!
//! [`assess`] runs 3-6 and FAILS CLOSED: a behavioural change with no usable graph scores
//! `no_graph_score` with the reason recorded. There is no quiet fallback to line counts.
//!
//! Every number and word list lives in [`THRESHOLDS`], so tuning the policy is a one-table edit.

use std::collections::{BTreeMap, BTreeSet};
use wicked_apps_core::{GraphRead, Node, NodeKind};
use wicked_estate_core::{SymbolId, TraversalSpec};

/// One non-docs file a diff touches, with the BASE-side lines its hunks touch: each removed line,
/// and the lines either side of an insertion. The graph is indexed at the base, so symbols are
/// looked up by `old_path` and `old_lines`; a new file has neither and no graph symbols.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TouchedFile {
    /// The behavioural side of the header (the `b/` side unless it is docs).
    pub path: String,
    /// The `a/` side, the path the graph indexed. Empty for a new file AND for a copy (its `a/`
    /// side is the unchanged source).
    pub old_path: String,
    pub old_lines: BTreeSet<u32>,
}

/// What a unit changed, read from its diff alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChangeSignals {
    pub lines_added: u32,
    pub lines_removed: u32,
    pub docs_files: u32,
    pub code_files: u32,
    pub test_files: u32,
    pub config_files: u32,
    /// A non-docs file sits in a critical subsystem ([`Thresholds::critical_path_markers`]).
    pub critical: bool,
    /// The change touches a destructive path: a delete, erase, force, migration, and so on.
    pub destructive: bool,
    /// The non-docs files, in diff order.
    pub touched: Vec<TouchedFile>,
    /// (QE waiver, complexity) Changed (`+`/`-`) lines of code and test files that carry a branch
    /// token ([`Thresholds::branch_tokens`]): the diff's own measure of how much control flow it
    /// touched. Estate exposes no per-symbol complexity metric, so this is read from the hunks.
    pub branch_lines: u32,
    /// (QE waiver, complexity) Changed (`+`/`-`) lines of the non-docs files.
    pub behavioural_lines: u32,
    /// (QE waiver, novelty) Dependencies the diff ADDS: a key added to a dependency section of a
    /// manifest, or a package entry added to a lockfile, that the same file does not also remove
    /// (a version bump removes and re-adds its key, so it is not new).
    pub new_dependencies: BTreeSet<String>,
    /// (QE waiver, novelty) Public or exported symbols the diff DECLARES that it does not also
    /// remove (a signature edit re-declares its name, so it is not new).
    pub new_public_symbols: BTreeSet<String>,
    /// (QE waiver, novelty) Touched pre-existing paths with little history before the run base:
    /// fewer than [`Thresholds::low_history_commits`] commits, or a history git could not read.
    /// Filled by [`with_history`], which needs the repository; a pure diff read leaves it 0.
    pub low_history: u32,
}

impl ChangeSignals {
    /// Anything but a docs-only change. Docs-only has no symbols and summons nothing.
    pub(crate) fn behavioural(&self) -> bool {
        self.code_files + self.test_files + self.config_files > 0 || self.destructive
    }
}

/// Whether the repo graph can speak for the run's base commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphAge {
    /// Indexed at the run's base commit, the commit the diff's old side is taken from.
    Current,
    /// Indexed at any other commit: older, newer, or unrelated.
    Stale { indexed: String, base: String },
    /// No graph, no recorded commit, or unreadable.
    Missing(String),
}

impl GraphAge {
    /// Why the graph cannot be used, or `None` when it can.
    pub(crate) fn reason(&self) -> Option<String> {
        match self {
            GraphAge::Current => None,
            GraphAge::Stale { indexed, base } => Some(format!(
                "graph indexed at {indexed} is not the run base {base}"
            )),
            GraphAge::Missing(why) => Some(format!("no graph: {why}")),
        }
    }
}

/// What the graph says about the change. Plain data, so the score is testable without a graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ImpactSignals {
    /// |C|: symbols whose lines the diff touches; a touched file with none (a rename, a mode
    /// change, lines outside every symbol) is its file node. A touched file the graph does not
    /// know (a new file) counts as one changed symbol with no dependents.
    pub changed_symbols: u32,
    /// |R|: distinct dependents of C within [`Thresholds::hops`], outside C.
    pub dependents: u32,
    /// Products (`crates/<x>`, `packages/<x>`, else the root) that C and R land in.
    pub products: u32,
    /// The diff touches a published surface (a path or a wire-type symbol marker).
    pub contract_change: bool,
    /// G: the share of the INDEXED part of C that no test symbol reaches, 0..=1. An unindexed
    /// path is not in it (core#711: unindexed is not untested — the graph cannot see what reaches
    /// a file it never indexed, so it is unknown, and unknown earns no points).
    pub test_gap: f32,
    /// Touched paths the graph does not know (new files, copies): each is one changed symbol
    /// with no dependents and an unknown test reach.
    pub unindexed: u32,
    pub critical: bool,
    pub destructive: bool,
    /// A traversal's node cap dropped a reachable node, so `dependents` is a lower bound
    /// (estate's `Subgraph::node_cap_reached`).
    pub node_cap_reached: bool,
    /// A traversal's hop horizon left dependents beyond [`Thresholds::hops`] uncounted
    /// (estate's `Subgraph::depth_horizon_reached`). `dependents` is exact within the horizon.
    pub depth_horizon_reached: bool,
    /// (complexity) Changed `+`/`-` lines of the non-docs files.
    pub lines_changed: u32,
    /// (complexity) [`ChangeSignals::branch_lines`].
    pub branch_lines: u32,
    /// (novelty) [`ChangeSignals::new_dependencies`], counted.
    pub new_dependencies: u32,
    /// (novelty) [`ChangeSignals::new_public_symbols`], counted.
    pub new_public_symbols: u32,
    /// (novelty) [`ChangeSignals::low_history`].
    pub low_history: u32,
}

impl ImpactSignals {
    /// Either cause: `dependents` is not the whole blast radius. The wire's `truncated` field.
    pub(crate) fn truncated(&self) -> bool {
        self.node_cap_reached || self.depth_horizon_reached
    }
}

/// The deterministic score and how it was reached, one line per contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Score {
    pub score: u8,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Depth {
    None,
    Standard,
    Deep,
}

/// The review a change gets: live monitors, how deep they read, and whether an independent
/// reviewer reads the finished change afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewPlan {
    pub monitors: u8,
    pub depth: Depth,
    pub post_hoc_reviewer: bool,
    /// The post-hoc reviewer runs on a different CLI than the worker.
    pub post_hoc_other_cli: bool,
}

/// A non-deterministic assessment that may raise the score. Not wired to any CLI here.
pub(crate) trait ModelAssessment {
    /// May ADD 0, 10 or 20 with a rationale. Anything else is clamped; nothing subtracts.
    fn assess(&self, signals: &ImpactSignals, deterministic: u8) -> Option<ModelBonus>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelBonus {
    pub add: u8,
    pub rationale: String,
}

/// The graph a run reads, with the commit its diff's old side is taken from, or why it cannot.
/// [`assess`] checks [`graph_age`] itself, so no caller can score against a graph whose spans
/// belong to another commit.
pub(crate) enum Graph<'a> {
    Ready {
        store: &'a dyn GraphRead,
        base_commit: &'a str,
    },
    Unavailable(String),
}

/// The full verdict for one change.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Assessment {
    /// The table's score, before the model hook.
    pub deterministic: u8,
    /// The final score the plan was read from.
    pub score: u8,
    pub reasons: Vec<String>,
    pub model: Option<ModelBonus>,
    /// `None` when the graph was unusable (the score is then `no_graph_score`).
    pub signals: Option<ImpactSignals>,
    pub plan: ReviewPlan,
}

/// The tunable policy. One table, so an operator changes the policy here and nowhere else.
pub(crate) struct Thresholds {
    /// How far dependents are followed from a changed symbol.
    pub hops: u32,
    /// `(min dependents, reach points)`, ascending. The first tier starts at 0: a behavioural
    /// change nothing depends on yet (a new leaf file) is still a change, and gets one monitor.
    pub reach_tiers: &'static [(u32, u32)],
    /// Points per product beyond the first, and the most such steps that count.
    pub product_step: u32,
    pub product_max_steps: u32,
    pub contract_points: u32,
    /// Scaled by the test gap; counted only when there are dependents.
    pub test_gap_points: u32,
    pub critical_points: u32,
    /// A destructive path scores at least this, however small the reach.
    pub destructive_floor: u32,
    /// A behavioural change with no usable graph scores this. Fail closed.
    pub no_graph_score: u8,
    /// (X1) The deterministic score of work with no repo (no graph to read): the lowest band.
    /// The PA's `RISK` rating can only raise it, through the model part.
    pub repo_less_baseline: u8,
    /// The model hook is consulted only from this deterministic score up.
    pub model_hook_min_score: u8,
    pub model_bonus_step: u8,
    pub model_bonus_max: u8,
    /// `(min score, plan)`, ascending.
    pub bands: &'static [(u8, ReviewPlan)],
    /// The floor of each band (DES-TEAMING-002 §8.5): one row per [`Thresholds::bands`] row, in
    /// the same order, so the monitor count, the floor and the high-risk rule are tuned together.
    pub floors: &'static [FloorRow],
    /// File extensions that are prose or images: never behavioural, wherever they live.
    pub docs_exts: &'static [&'static str],
    /// File extensions that are docs only under a [`Thresholds::docs_dirs`] directory (an HTML
    /// design doc or prototype, a diagram); anywhere else they are code (a web app's
    /// `index.html`, an SVG asset, which is markup and can carry script).
    pub docs_dir_exts: &'static [&'static str],
    /// Path components that mark a docs directory.
    pub docs_dirs: &'static [&'static str],
    /// File extensions that are configuration.
    pub config_exts: &'static [&'static str],
    /// Path-token prefixes of critical subsystems.
    pub critical_path_markers: &'static [&'static str],
    /// Substrings (lowercased) of a hunk line (changed or context) in a non-docs file that mark a
    /// destructive path.
    pub destructive_line_markers: &'static [&'static str],
    /// Path-token prefixes that make a whole file destructive.
    pub destructive_path_markers: &'static [&'static str],
    /// Substrings (lowercased) of a touched path that is a published surface.
    pub contract_path_markers: &'static [&'static str],
    /// Substrings (lowercased) of a changed TYPE's name that make it a wire type.
    pub contract_symbol_markers: &'static [&'static str],
    /// A symbol whose name starts with one of these is a test, wherever it lives.
    pub test_name_prefixes: &'static [&'static str],
    /// (complexity) `(min changed lines, points)`, ascending; the first tier is the lowest band.
    pub complexity_line_tiers: &'static [(u32, u32)],
    /// (complexity) `(min branch lines, points)`, ascending.
    pub complexity_branch_tiers: &'static [(u32, u32)],
    /// (complexity) `(min changed symbols, points)`, ascending.
    pub complexity_symbol_tiers: &'static [(u32, u32)],
    /// The most the complexity terms add together.
    pub complexity_max: u32,
    /// Lowercase tokens that mark a changed line as control flow (matched on word boundaries).
    pub branch_tokens: &'static [&'static str],
    /// (novelty) Points per new or unindexed file, and the most such steps that count. `unindexed`
    /// used to earn nothing here (core#711 keeps it out of the TEST GAP, which is unchanged); a
    /// file the base graph has never seen is new code with no history, so it earns novelty.
    pub novelty_file_step: u32,
    pub novelty_file_max_steps: u32,
    /// (novelty) Any new dependency.
    pub novelty_dependency_points: u32,
    /// (novelty) Any new public or exported symbol.
    pub novelty_public_symbol_points: u32,
    /// (novelty) Any touched path with little history ([`Self::low_history_commits`]).
    pub novelty_low_history_points: u32,
    /// A pre-existing path with fewer commits than this before the base has little history.
    pub low_history_commits: u32,
    /// The most the novelty terms add together.
    pub novelty_max: u32,
    /// (QE acceptance waiver) The highest final score a run's QE acceptance may be WAIVED at:
    /// the score a behavioural change earns for reach alone (the first [`Self::reach_tiers`]
    /// row), so a waiver needs every dimension in its lowest band — no complexity and no
    /// novelty points, and no span, contract, test-gap, critical or destructive term.
    pub qe_waiver_max_score: u8,
}

/// One band's floor (DES-TEAMING-002 §8.5): the minimum phase types, in order, and whether the
/// band is high risk. The phase types are catalog ids (`crate::catalog::CATALOG_IDS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloorRow {
    pub phases: &'static [&'static str],
    pub high_risk: bool,
}

/// A score's floor, read from [`THRESHOLDS`]: the band it lands in (`"40-69"`), the band's
/// minimum phase types, and the high-risk rule (§8.5: the band's row says so, OR the change is
/// destructive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Floor {
    pub band: String,
    pub phases: Vec<&'static str>,
    pub high_risk: bool,
}

/// The reason an intent score gives a creator plan that declares no touch set (§8.2, §8.4): it
/// scores `no_graph_score`, S4's fail-closed rule.
pub(crate) const NO_DECLARED_SCOPE: &str = "no declared scope";

const PLAN_NONE: ReviewPlan = ReviewPlan {
    monitors: 0,
    depth: Depth::None,
    post_hoc_reviewer: false,
    post_hoc_other_cli: false,
};
const PLAN_STANDARD: ReviewPlan = ReviewPlan {
    monitors: 1,
    depth: Depth::Standard,
    post_hoc_reviewer: false,
    post_hoc_other_cli: false,
};
const PLAN_DEEP: ReviewPlan = ReviewPlan {
    monitors: 2,
    depth: Depth::Deep,
    post_hoc_reviewer: true,
    post_hoc_other_cli: false,
};
const PLAN_MOST: ReviewPlan = ReviewPlan {
    monitors: 3,
    depth: Depth::Deep,
    post_hoc_reviewer: true,
    post_hoc_other_cli: true,
};

/// The policy's values, and why (operator decision on #590, 2026-09-23).
///
/// - **Reach is the base.** What breaks when this symbol changes is what its dependents are, so
///   reach is read from the graph: 20 for up to five dependents, 40 to twenty, 60 to a hundred,
///   80 beyond. A behavioural change with no dependents still gets the first tier: 13/13
///   behavioural PRs in this program came back with findings, so no behavioural change goes
///   unwatched. Docs-only has no symbols and scores 0: 6/6 docs PRs landed clean first time.
/// - **Span** (+10 per product beyond the first, at most +20): "correct locally, wrong for the
///   system" is the likely failure once a change crosses a product boundary.
/// - **Contract** (+20): a published surface (api-types, `CoreEvent`, MCP tool schemas, a
///   public API) has consumers this repo's graph cannot see.
/// - **Test gap** (+20 x G, only when there are dependents): the share of changed symbols no test
///   reaches. A leaf nothing depends on has nothing to regress.
/// - **Critical** (+20): the path markers that name the subsystems where a defect costs the most.
/// - **Destructive floor 70**: the issue's HIGH was a one-handler defect before a memory erase
///   that survived the creator, the evaluator, a human gate and a PR. A destructive path gets the
///   top band whatever its reach, and it is detected from the diff alone so it works with no graph.
/// - **No graph, or a graph not indexed at the run's base, scores 100.** A policy that cannot see
///   the dependents, or sees them at another commit's line numbers, must not guess low.
/// - **Bands**: 0-19 nothing; 20-39 one standard monitor; 40-69 two deep monitors and a post-hoc
///   reviewer; 70-100 three deep monitors and a post-hoc reviewer on a different CLI than the
///   worker. Three is the ceiling: a monitor that flags everything is noise (issue risk 2), and
///   concurrent seats starve each other.
/// - **Model hook**: may add 0, 10 or 20 with a rationale, from 20 up. Never subtracts, so the
///   deterministic score is a floor.
/// - **Complexity** (operator correction 2026-10-10, QE waiver): the size of the change, read from
///   the diff — changed lines (+10 from 51, +20 from 201), branch lines (+10 from 6, +20 from 21)
///   and changed symbols (+10 from 4), at most +30. Estate exposes no symbol complexity metric, so
///   the branch count stands in for it. A path-only intent has no lines and scores none here.
/// - **Novelty**: +10 per new or unindexed file (at most +20), +20 for any new dependency, +10 for
///   any new public or wire symbol, +10 for any touched path with under three commits of history;
///   at most +40. Prior memories or rules for the area are NOT read (no cheap estate call at the
///   scoring seam).
/// - **QE waiver at 20**: a run's QE acceptance is waived only at a final score of 20 or less AND
///   no complexity or novelty points — every dimension in its lowest band.
pub(crate) const THRESHOLDS: Thresholds = Thresholds {
    hops: 3,
    reach_tiers: &[(0, 20), (6, 40), (21, 60), (101, 80)],
    product_step: 10,
    product_max_steps: 2,
    contract_points: 20,
    test_gap_points: 20,
    critical_points: 20,
    destructive_floor: 70,
    no_graph_score: 100,
    repo_less_baseline: 0,
    model_hook_min_score: 20,
    model_bonus_step: 10,
    model_bonus_max: 20,
    bands: &[
        (0, PLAN_NONE),
        (20, PLAN_STANDARD),
        (40, PLAN_DEEP),
        (70, PLAN_MOST),
    ],
    floors: &[
        FloorRow {
            phases: &["build", "deliver"],
            high_risk: false,
        },
        FloorRow {
            phases: &["build", "review", "deliver"],
            high_risk: false,
        },
        FloorRow {
            phases: &["test_plan", "design", "build", "review", "deliver"],
            high_risk: false,
        },
        FloorRow {
            phases: &[
                "test_plan",
                "design",
                "architecture",
                "build",
                "review",
                "security_review",
                "deliver",
            ],
            high_risk: true,
        },
    ],
    docs_exts: &[
        "md", "mdx", "markdown", "rst", "adoc", "txt", "png", "jpg", "jpeg", "gif", "webp",
    ],
    docs_dir_exts: &["html", "htm", "svg"],
    docs_dirs: &["docs", "doc"],
    config_exts: &["toml", "json", "yaml", "yml", "lock", "ini", "cfg", "env"],
    critical_path_markers: &[
        "memory",
        "gate",
        "governance",
        "fence",
        "deliver",
        "migration",
        "credential",
        "secret",
        "state_home",
        "path_policy",
        "write_posture",
    ],
    // Grouped by family (review on #600: listing `remove_dir_all` without `remove_dir` let
    // `std::fs::remove_dir(path)?` through). Matched as lowercase substrings, so one entry covers
    // its longer forms: `remove_dir` covers `remove_dir_all`, `unlink` covers `unlinkSync` and
    // `os.unlink`, `rmdir` covers `fs.rmdir` and `os.rmdir`, `rm -r` covers `rm -rf`.
    destructive_line_markers: &[
        // Rust std::fs
        "remove_dir",
        "remove_file",
        // Node fs and rimraf
        "fs.rm(",
        "rmsync(",
        "rmdir",
        "unlink",
        "rimraf",
        // Python os and shutil
        "os.remove(",
        "shutil.rmtree(",
        // shell
        "rm -r",
        "rm -f",
        // git
        "git clean",
        "branch -d",
        "push --force",
        "push -f",
        "reset --hard",
        "worktree remove",
        "--force",
        // SQL
        "drop table",
        "drop column",
        "drop index",
        "drop database",
        "drop schema",
        "truncate table",
        "delete from",
        // generic verbs, any language
        "erase",
        "purge",
        "wipe",
    ],
    destructive_path_markers: &["migration"],
    contract_path_markers: &[
        "api-types",
        "api_types",
        "/event.rs",
        "/events.",
        "/events/",
        "schema",
        "openapi",
        "protocol",
        "/mcp",
        ".d.ts",
        "wire",
    ],
    contract_symbol_markers: &[
        "event", "schema", "tool", "api", "request", "response", "dto", "payload",
    ],
    test_name_prefixes: &["test"],
    complexity_line_tiers: &[(0, 0), (51, 10), (201, 20)],
    complexity_branch_tiers: &[(0, 0), (6, 10), (21, 20)],
    complexity_symbol_tiers: &[(0, 0), (4, 10)],
    complexity_max: 30,
    branch_tokens: &[
        "if", "else", "elif", "match", "case", "switch", "for", "while", "loop", "catch", "except",
        "try", "&&", "||", "?",
    ],
    novelty_file_step: 10,
    novelty_file_max_steps: 2,
    novelty_dependency_points: 20,
    novelty_public_symbol_points: 10,
    novelty_low_history_points: 10,
    low_history_commits: 3,
    novelty_max: 40,
    qe_waiver_max_score: 20,
};

// The QE waiver line is the lowest reach tier's score: a waiver means reach alone and nothing else.
const _: () = assert!(THRESHOLDS.qe_waiver_max_score as u32 == THRESHOLDS.reach_tiers[0].1);

// One floor row per band: a length mismatch is a build error, not a silent missing floor.
const _: () = assert!(THRESHOLDS.bands.len() == THRESHOLDS.floors.len());

/// Whether the graph at `store` speaks for `base_commit`: ONE rule, the indexed commit IS the base
/// commit. The diff's old side is the base (`repo::create_worktree_based` checks the run tree out
/// at `base.commit`, and the run diff is `base_commit..run_branch`), and [`impact_signals`] maps
/// hunks by base-side path and line, so the graph's spans must be the base's spans. A graph at a
/// DESCENDANT is not enough: an insertion above a hot symbol shifts its span, the base-side lookup
/// misses it, and a hot edit scores as a leaf (review on #600). The engine never re-indexes at run
/// start; the graph is whatever onboarding indexed the registered root at, which the base lift
/// (`RunBase::lifted`) can leave behind. Anything but equality is stale, and stale scores 100.
pub(crate) fn graph_age<S: GraphRead + ?Sized>(store: &S, base_commit: &str) -> GraphAge {
    let indexed = match store.repo_info() {
        Err(e) => return GraphAge::Missing(format!("repo info unreadable: {e}")),
        Ok(None) => return GraphAge::Missing("the graph records no repo info".into()),
        Ok(Some(info)) => match info.commit {
            Some(c) if !c.is_empty() => c,
            _ => return GraphAge::Missing("the graph records no commit".into()),
        },
    };
    if indexed == base_commit {
        GraphAge::Current
    } else {
        GraphAge::Stale {
            indexed,
            base: base_commit.to_string(),
        }
    }
}

/// Read C, R, span, contract and test gap for `diff` from the graph.
pub(crate) fn impact_signals<S: GraphRead + ?Sized>(
    store: &S,
    diff: &ChangeSignals,
) -> anyhow::Result<ImpactSignals> {
    let t = &THRESHOLDS;
    if !diff.behavioural() {
        return Ok(ImpactSignals {
            critical: diff.critical,
            destructive: diff.destructive,
            ..Default::default()
        });
    }
    let mut s = ImpactSignals {
        critical: diff.critical,
        destructive: diff.destructive,
        lines_changed: diff.behavioural_lines,
        branch_lines: diff.branch_lines,
        new_dependencies: diff.new_dependencies.len() as u32,
        new_public_symbols: diff.new_public_symbols.len() as u32,
        low_history: diff.low_history,
        ..Default::default()
    };
    let nodes = store.all_nodes()?;
    let mut by_file: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
    for n in &nodes {
        by_file.entry(n.location.file.as_str()).or_default().push(n);
    }
    let mut seeds: Vec<SymbolId> = Vec::new();
    let mut seed_ids: BTreeSet<&str> = BTreeSet::new();
    let mut products: BTreeSet<String> = BTreeSet::new();
    // Touched files with no indexed symbol: each is one changed symbol nothing reaches.
    let mut unindexed = 0u32;
    for f in &diff.touched {
        products.insert(product(&f.path));
        s.contract_change |= has_marker(&f.path, t.contract_path_markers)
            || has_marker(&f.old_path, t.contract_path_markers);
        let in_file: &[&Node] = by_file
            .get(f.old_path.as_str())
            .map_or(&[], |v| v.as_slice());
        let overlaps = |n: &Node| {
            let sp = &n.location.span;
            f.old_lines
                .iter()
                .any(|l| sp.start_line <= *l && *l <= sp.end_line)
        };
        // The symbols whose lines the diff touches. Otherwise the file node stands for the file:
        // touched lines outside every symbol (top-level statements, imports), or NO touched lines
        // at all (a pure rename/move, a mode change), whose importers still count (review on
        // #600: a rename of a heavily imported module read as a leaf). Only an old path the
        // graph does not know (a new file) is unindexed.
        let mut hits: Vec<&Node> = in_file
            .iter()
            .copied()
            .filter(|n| n.kind != NodeKind::File && overlaps(n))
            .collect();
        if hits.is_empty() {
            hits = in_file
                .iter()
                .copied()
                .filter(|n| n.kind == NodeKind::File)
                .collect();
        }
        if hits.is_empty() {
            unindexed += 1;
            continue;
        }
        for n in hits {
            s.contract_change |= is_type(&n.kind) && has_marker(&n.name, t.contract_symbol_markers);
            if seed_ids.insert(n.symbol.0.as_str()) {
                seeds.push(n.symbol.clone());
            }
        }
    }
    s.changed_symbols = seeds.len() as u32 + unindexed;
    s.unindexed = unindexed;
    // Per seed, so the test gap is attributable. R is the union outside C. An unindexed path is
    // not untested, it is unknown: it has no seed, so no traversal and no share of the gap.
    let spec = TraversalSpec::blast_radius(t.hops);
    let mut reached: BTreeSet<String> = BTreeSet::new();
    let mut untested = 0u32;
    for seed in &seeds {
        let sub = store.traverse(seed, &spec)?;
        s.node_cap_reached |= sub.node_cap_reached;
        s.depth_horizon_reached |= sub.depth_horizon_reached;
        let mut tested = false;
        for n in &sub.nodes {
            if !sub.depths.contains_key(&n.symbol.0) || seed_ids.contains(n.symbol.0.as_str()) {
                continue;
            }
            tested |= is_test_symbol(n);
            products.insert(product(&n.location.file));
            reached.insert(n.symbol.0.clone());
        }
        untested += u32::from(!tested);
    }
    s.dependents = reached.len() as u32;
    s.products = products.len() as u32;
    s.test_gap = if seeds.is_empty() {
        0.0
    } else {
        untested as f32 / seeds.len() as f32
    };
    Ok(s)
}

/// The test-gap reason's note on unindexed paths: the graph has no edge to read for them, so
/// their test reach is unknown — never "untested".
fn unindexed_note(unindexed: u32) -> String {
    format!("{unindexed} unindexed path(s) with unknown test reach")
}

/// The reach reason's note on an incomplete traversal, by cause (core#692): a node cap makes
/// `dependents` a lower bound ("capped"); a hop horizon only means dependents exist further out
/// than the table counts ("depth-limited"). Both can hold.
fn truncation_note(s: &ImpactSignals) -> &'static str {
    match (s.node_cap_reached, s.depth_horizon_reached) {
        (true, true) => " (capped, depth-limited)",
        (true, false) => " (capped)",
        (false, true) => " (depth-limited)",
        (false, false) => "",
    }
}

/// The deterministic score from the table.
pub(crate) fn impact_score(s: &ImpactSignals) -> Score {
    impact_score_in(&THRESHOLDS, s)
}

/// [`impact_score`] against a given table (a test tunes one value of [`THRESHOLDS`]).
pub(crate) fn impact_score_in(t: &Thresholds, s: &ImpactSignals) -> Score {
    let mut reasons = Vec::new();
    let mut score: u32 = 0;
    if s.changed_symbols == 0 {
        reasons.push("no changed symbols".to_string());
    } else {
        let reach = t
            .reach_tiers
            .iter()
            .rev()
            .find(|(min, _)| s.dependents >= *min)
            .map_or(0, |(_, p)| *p);
        score += reach;
        reasons.push(format!(
            "reach {reach}: {} changed symbol(s){}, {} dependent(s) within {} hops{}",
            s.changed_symbols,
            if s.unindexed > 0 {
                format!(" ({} unindexed)", s.unindexed)
            } else {
                String::new()
            },
            s.dependents,
            t.hops,
            truncation_note(s)
        ));
    }
    let span = s.products.saturating_sub(1).min(t.product_max_steps) * t.product_step;
    if span > 0 {
        score += span;
        reasons.push(format!("span +{span}: {} products", s.products));
    }
    if s.contract_change {
        score += t.contract_points;
        reasons.push(format!(
            "contract +{}: a published surface changed",
            t.contract_points
        ));
    }
    if s.dependents > 0 && s.test_gap > 0.0 {
        let gap = (t.test_gap_points as f32 * s.test_gap.clamp(0.0, 1.0)).round() as u32;
        score += gap;
        reasons.push(format!(
            "test gap +{gap}: {:.0}% of indexed changed symbols reached by no test{}",
            s.test_gap * 100.0,
            if s.unindexed > 0 {
                format!("; {}", unindexed_note(s.unindexed))
            } else {
                String::new()
            }
        ));
    } else if s.unindexed > 0 {
        // No gap scored, but say why the unindexed part is not in it (core#711: the operator
        // read "100% untested" where the graph only had no edge to read).
        reasons.push(format!("test gap +0: {}", unindexed_note(s.unindexed)));
    }
    if s.critical {
        score += t.critical_points;
        reasons.push(format!(
            "critical +{}: a critical-path file changed",
            t.critical_points
        ));
    }
    let (complexity, novelty) = dimension_terms(t, s);
    for (points, why) in complexity.iter().chain(novelty.iter()) {
        score += points;
        reasons.push(why.clone());
    }
    if s.destructive && score < t.destructive_floor {
        score = t.destructive_floor;
        reasons.push(format!(
            "destructive floor {}: a destructive path changed",
            t.destructive_floor
        ));
    }
    Score {
        score: score.min(100) as u8,
        reasons,
    }
}

/// The tier points of `value` in `(min, points)` tiers (0 below the first).
fn tier(tiers: &[(u32, u32)], value: u32) -> u32 {
    tiers
        .iter()
        .rev()
        .find(|(min, _)| value >= *min)
        .map_or(0, |(_, p)| *p)
}

/// The complexity and novelty terms of `s`: one reason line per term with points > 0, each
/// dimension capped at [`Thresholds::complexity_max`] / [`Thresholds::novelty_max`] (the later
/// terms absorb the cap).
fn dimension_terms(t: &Thresholds, s: &ImpactSignals) -> (Vec<(u32, String)>, Vec<(u32, String)>) {
    fn capped(dim: &str, terms: Vec<(u32, String)>, max: u32) -> Vec<(u32, String)> {
        let mut left = max;
        terms
            .into_iter()
            .filter_map(|(p, label)| {
                let p = p.min(left);
                left -= p;
                (p > 0).then(|| (p, format!("{dim} +{p}: {label}")))
            })
            .collect()
    }
    let complexity = vec![
        (
            tier(t.complexity_line_tiers, s.lines_changed),
            format!("{} changed line(s)", s.lines_changed),
        ),
        (
            tier(t.complexity_branch_tiers, s.branch_lines),
            format!("{} changed branch line(s)", s.branch_lines),
        ),
        (
            tier(t.complexity_symbol_tiers, s.changed_symbols),
            format!("{} changed symbol(s)", s.changed_symbols),
        ),
    ];
    let any = |n: u32, points: u32| if n > 0 { points } else { 0 };
    let novelty = vec![
        (
            s.unindexed.min(t.novelty_file_max_steps) * t.novelty_file_step,
            format!("{} new or unindexed file(s)", s.unindexed),
        ),
        (
            any(s.new_dependencies, t.novelty_dependency_points),
            format!("{} new dependency(ies)", s.new_dependencies),
        ),
        (
            any(s.new_public_symbols, t.novelty_public_symbol_points),
            format!("{} new public or wire symbol(s)", s.new_public_symbols),
        ),
        (
            any(s.low_history, t.novelty_low_history_points),
            format!(
                "{} touched path(s) with under {} commits of history",
                s.low_history, t.low_history_commits
            ),
        ),
    ];
    (
        capped("complexity", complexity, t.complexity_max),
        capped("novelty", novelty, t.novelty_max),
    )
}

/// (QE acceptance) Whether an assessment lets a run's QE acceptance be waived: a deterministic
/// read of the graph (never the fail-closed score), a final score at most
/// [`Thresholds::qe_waiver_max_score`], and no complexity or novelty points — every dimension in
/// its lowest band. `Err` carries why it is required, in the score's own words.
pub(crate) fn qe_waivable(a: &Assessment) -> Result<(), String> {
    let t = &THRESHOLDS;
    let Some(s) = a.signals.as_ref() else {
        return Err(format!(
            "impact score {} (the change could not be read: {})",
            a.score,
            a.reasons.join("; ")
        ));
    };
    let (complexity, novelty) = dimension_terms(t, s);
    if a.score > t.qe_waiver_max_score || !complexity.is_empty() || !novelty.is_empty() {
        return Err(format!(
            "impact score {} above the waiver line {} ({})",
            a.score,
            t.qe_waiver_max_score,
            a.reasons.join("; ")
        ));
    }
    Ok(())
}

/// The plan a score lands in.
pub(crate) fn plan_for(score: u8) -> ReviewPlan {
    THRESHOLDS
        .bands
        .iter()
        .rev()
        .find(|(min, _)| score >= *min)
        .map_or(PLAN_NONE, |(_, p)| *p)
}

/// The floor a score lands in (DES-TEAMING-002 §8.5), from [`THRESHOLDS`].
pub(crate) fn floor_for(score: u8, destructive: bool) -> Floor {
    floor_in(&THRESHOLDS, score, destructive)
}

/// [`floor_for`] against a given table. High risk is stated once, here: the band's row says so,
/// or the change is destructive.
pub(crate) fn floor_in(t: &Thresholds, score: u8, destructive: bool) -> Floor {
    let i = t
        .bands
        .iter()
        .rposition(|(min, _)| score >= *min)
        .unwrap_or(0);
    let lo = t.bands[i].0;
    let hi = t.bands.get(i + 1).map_or(100, |(next, _)| next - 1);
    let row = t.floors[i];
    Floor {
        band: format!("{lo}-{hi}"),
        phases: row.phases.to_vec(),
        high_risk: row.high_risk || destructive,
    }
}

/// The intent score of a plan (DES-TEAMING-002 §8.2, §8.4), before anything has run: the plan's
/// declared touch set, read as [`signals_from_paths`], through [`assess`]. One rule for every
/// author: a plan with a creator step (`build` or `produce`) and a missing or empty `touch`
/// scores `no_graph_score` with the reason [`NO_DECLARED_SCOPE`]; a plan with no creator step
/// and no `touch` scores 0.
pub(crate) fn assess_intent(
    creator: bool,
    touch: Option<&[&str]>,
    graph: Graph<'_>,
    hook: Option<&dyn ModelAssessment>,
) -> Assessment {
    let t = &THRESHOLDS;
    let fixed = |score: u8, reason: &str, signals: Option<ImpactSignals>| Assessment {
        deterministic: score,
        score,
        reasons: vec![reason.to_string()],
        model: None,
        signals,
        plan: plan_for(score),
    };
    match touch {
        Some(paths) if !paths.is_empty() => assess(&signals_from_paths(paths), graph, hook),
        _ if creator => fixed(t.no_graph_score, NO_DECLARED_SCOPE, None),
        _ => fixed(
            0,
            "no creator step and no declared scope",
            Some(ImpactSignals::default()),
        ),
    }
}

/// The one policy entry point: signals -> score -> optional model bonus -> plan. Fails closed on
/// a behavioural change with no usable graph.
pub(crate) fn assess(
    diff: &ChangeSignals,
    graph: Graph<'_>,
    hook: Option<&dyn ModelAssessment>,
) -> Assessment {
    let t = &THRESHOLDS;
    let (mut score, mut reasons, signals) = if !diff.behavioural() {
        (
            0,
            vec!["docs-only: no symbols".to_string()],
            Some(ImpactSignals::default()),
        )
    } else {
        let read = match graph {
            Graph::Ready { store, base_commit } => match graph_age(store, base_commit).reason() {
                Some(reason) => Err(reason),
                None => impact_signals(store, diff).map_err(|e| format!("graph read failed: {e}")),
            },
            Graph::Unavailable(reason) => Err(reason),
        };
        match read {
            Ok(s) => {
                let sc = impact_score(&s);
                (sc.score, sc.reasons, Some(s))
            }
            Err(reason) => (
                t.no_graph_score,
                vec![format!("fail closed at {}: {reason}", t.no_graph_score)],
                None,
            ),
        }
    };
    let deterministic = score;
    let mut model = None;
    if let (Some(hook), Some(s)) = (hook, signals.as_ref()) {
        if deterministic >= t.model_hook_min_score {
            if let Some(b) = hook.assess(s, deterministic) {
                let add = b.add.min(t.model_bonus_max) / t.model_bonus_step * t.model_bonus_step;
                score = score.saturating_add(add).min(100);
                reasons.push(format!("model +{add}: {}", b.rationale));
                model = Some(ModelBonus {
                    add,
                    rationale: b.rationale,
                });
            }
        }
    }
    Assessment {
        deterministic,
        score,
        reasons,
        model,
        signals,
        plan: plan_for(score),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Docs,
    Test,
    Config,
    Code,
}

/// Derive [`ChangeSignals`] from a unified diff (`git diff` output).
///
/// A `diff --git` line only DELIMITS a file (review on #600: splitting it on the first ` b/`
/// misread `src/a b/core.rs`). A file's paths come from its `---`/`+++` headers (git-unquoted;
/// `/dev/null` is an absent side: new or deleted) and, for a hunk-less rename or copy, from the
/// `rename from`/`rename to` (`copy from`/`copy to`) lines. A rename's old path is the path the
/// graph indexed, so its importers count; a copy's source did not change, so the copy is scored
/// as a new leaf (`old_path` empty) while the source still classifies it and carries its path
/// markers (core#611).
///
/// Fails closed: a file is behavioural if EITHER side is (a rename from `src/memory.rs` to
/// `docs/memory.md` moves code out of a critical subsystem), a block that names no path counts
/// as code, and destructive markers are read from a behavioural hunk's context lines as well as
/// its `+`/`-` lines (a guard change around an existing destructive call). Only `+`/`-` lines
/// count toward `lines_added`/`lines_removed`.
pub(crate) fn signals_from_diff(diff: &str) -> ChangeSignals {
    let mut s = ChangeSignals::default();
    // Split into per-file blocks. Lines before the first header form a headerless block.
    let mut blocks: Vec<Vec<&str>> = vec![Vec::new()];
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            blocks.push(Vec::new());
        } else if let Some(block) = blocks.last_mut() {
            block.push(line);
        }
    }
    for block in &blocks {
        file_block(block, &mut s);
    }
    s
}

/// Derive [`ChangeSignals`] from a predicted touch set (DES-TEAMING-002 §8.2): the paths a plan
/// declares it will change, with no diff yet. Each path is classified as [`signals_from_diff`]
/// classifies a header path; a non-docs path is touched as a whole file (no lines, so its file
/// node stands for it and its importers count), and the critical and destructive PATH markers
/// apply. No line markers: there are no lines.
pub(crate) fn signals_from_paths(paths: &[&str]) -> ChangeSignals {
    let t = &THRESHOLDS;
    let mut s = ChangeSignals::default();
    for p in paths {
        match classify(p) {
            Kind::Docs => {
                s.docs_files += 1;
                continue;
            }
            Kind::Test => s.test_files += 1,
            Kind::Config => s.config_files += 1,
            Kind::Code => s.code_files += 1,
        }
        s.critical |= has_token(p, t.critical_path_markers);
        s.destructive |= has_token(p, t.destructive_path_markers);
        s.touched.push(TouchedFile {
            path: p.to_string(),
            old_path: p.to_string(),
            old_lines: BTreeSet::new(),
        });
    }
    s
}

/// One file's header lines and hunks.
fn file_block(lines: &[&str], s: &mut ChangeSignals) {
    let t = &THRESHOLDS;
    let hunks = lines
        .iter()
        .position(|l| l.starts_with("@@"))
        .unwrap_or(lines.len());
    let (mut old, mut new) = (None::<String>, None::<String>);
    let mut deleted = false;
    // A copy's `a/` side is the SOURCE, which did not change (core#611): the source path still
    // classifies the block and carries its critical/destructive markers, but it is not the path
    // the graph is asked about — the copy is a new, unindexed leaf.
    let mut copied = false;
    for l in &lines[..hunks] {
        if let Some(p) = l.strip_prefix("--- ") {
            old = Some(header_path(p, "a/"));
        } else if let Some(p) = l.strip_prefix("+++ ") {
            let p = header_path(p, "b/");
            deleted |= p.is_empty();
            new = Some(p);
        } else if let Some(p) = l
            .strip_prefix("rename from ")
            .or_else(|| l.strip_prefix("copy from "))
        {
            copied |= l.starts_with("copy from ");
            old.get_or_insert_with(|| git_unquote(p));
        } else if let Some(p) = l
            .strip_prefix("rename to ")
            .or_else(|| l.strip_prefix("copy to "))
        {
            copied |= l.starts_with("copy to ");
            new.get_or_insert_with(|| git_unquote(p));
        } else if l.starts_with("deleted file mode") {
            deleted = true;
        }
    }
    let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());
    let sides = [old.as_str(), new.as_str()];
    let named = sides.iter().any(|p| !p.is_empty());
    if !named && hunks == lines.len() {
        return; // nothing to attribute: no path and no hunk
    }
    // A dependency manifest is never docs, whatever its extension (`requirements.txt`): it is
    // configuration, and its adds are read for new dependencies below.
    let manifest = sides.iter().rev().find_map(|p| manifest_kind(p));
    let code: Vec<&str> = sides
        .iter()
        .copied()
        .filter(|p| !p.is_empty() && (classify(p) != Kind::Docs || manifest_kind(p).is_some()))
        .collect();
    let k = match code.last() {
        Some(p) if manifest_kind(p).is_some() && classify(p) == Kind::Docs => Kind::Config,
        Some(p) => classify(p),
        None if !named => Kind::Code,
        None => Kind::Docs,
    };
    match k {
        Kind::Docs => s.docs_files += 1,
        Kind::Test => s.test_files += 1,
        Kind::Config => s.config_files += 1,
        Kind::Code => s.code_files += 1,
    }
    let behavioural = k != Kind::Docs;
    if behavioural {
        for p in sides {
            s.critical |= has_token(p, t.critical_path_markers);
            s.destructive |= has_token(p, t.destructive_path_markers);
        }
        s.destructive |= deleted;
        s.touched.push(TouchedFile {
            path: code.last().copied().unwrap_or("").to_string(),
            old_path: if copied { String::new() } else { old.clone() },
            old_lines: BTreeSet::new(),
        });
    }
    // Base-side line number of the next old line in the current hunk.
    let mut old_next: u32 = 0;
    // (QE waiver) The novelty keys each side of the file declares: dependency keys of a manifest
    // or lockfile, public symbols of a code file. Only what the `+` side adds and the `-` side
    // does not remove is new.
    let (mut deps_added, mut deps_removed) = (BTreeSet::new(), BTreeSet::new());
    let (mut pub_added, mut pub_removed) = (BTreeSet::new(), BTreeSet::new());
    let mut section = None::<String>;
    let source_ext = code
        .last()
        .and_then(|p| p.rsplit('/').next())
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase());
    for line in &lines[hunks..] {
        if let Some(h) = line.strip_prefix("@@") {
            // `@@ -a[,b] +c[,d] @@`: `a` is the first base-side line (0 for a new file).
            old_next = h
                .trim_start()
                .strip_prefix('-')
                .and_then(|r| r.split([',', ' ']).next())
                .and_then(|a| a.parse().ok())
                .unwrap_or(0);
            continue;
        }
        let (added, removed) = (line.starts_with('+'), line.starts_with('-'));
        // Some tools strip the space off an empty context line.
        let context = line.starts_with(' ') || line.is_empty();
        s.lines_added += u32::from(added);
        s.lines_removed += u32::from(removed);
        let body = line.get(1..).unwrap_or("");
        if let Some(m) = manifest {
            if let Some(key) = dependency_key(m, body, &mut section) {
                if added {
                    deps_added.insert(key);
                } else if removed {
                    deps_removed.insert(key);
                }
            }
        }
        if behavioural && (added || removed) {
            s.behavioural_lines += 1;
            if matches!(k, Kind::Code | Kind::Test) {
                s.branch_lines += u32::from(is_branch_line(body));
                if k == Kind::Code {
                    if let Some(name) = public_decl(source_ext.as_deref(), body) {
                        if added {
                            pub_added.insert(name);
                        } else {
                            pub_removed.insert(name);
                        }
                    }
                }
            }
        }
        // Context lines are scanned too (review on #600: a change that only loosens the guard
        // around an existing destructive call leaves the call itself on a context line).
        if behavioural && (added || removed || context) {
            let text = line.get(1..).unwrap_or("").to_ascii_lowercase();
            s.destructive |= t.destructive_line_markers.iter().any(|m| text.contains(m));
            // A copy's hunk lines are offsets into the unchanged source: nothing to look up.
            if let Some(f) = s.touched.last_mut().filter(|_| !copied) {
                if removed {
                    f.old_lines.insert(old_next);
                } else if added {
                    // An insertion touches the symbol either side of it.
                    f.old_lines.extend(
                        [old_next.saturating_sub(1), old_next]
                            .into_iter()
                            .filter(|l| *l > 0),
                    );
                }
            }
        }
        if removed || context {
            old_next += 1;
        }
    }
    s.new_dependencies
        .extend(deps_added.difference(&deps_removed).cloned());
    s.new_public_symbols
        .extend(pub_added.difference(&pub_removed).cloned());
}

/// A dependency manifest or lockfile, by file name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Manifest {
    /// `Cargo.toml`, `pyproject.toml`: keys of a `[...dependencies...]` section.
    Toml,
    /// `package.json`: `"name": "<range>"` entries.
    PackageJson,
    /// `go.mod`: `require` lines.
    GoMod,
    /// `requirements*.txt`: one requirement per line.
    Requirements,
    /// `Cargo.lock`, `uv.lock`, `poetry.lock`: `name = "<pkg>"` entries.
    TomlLock,
    /// `package-lock.json`, `npm-shrinkwrap.json`: `"node_modules/<pkg>": {` entries.
    NpmLock,
    /// `go.sum`: `<module> <version> h1:…` lines.
    GoSum,
}

fn manifest_kind(path: &str) -> Option<Manifest> {
    let name = path.rsplit('/').next().unwrap_or(path);
    Some(match name {
        "Cargo.toml" | "pyproject.toml" => Manifest::Toml,
        "package.json" => Manifest::PackageJson,
        "go.mod" => Manifest::GoMod,
        "Cargo.lock" | "uv.lock" | "poetry.lock" => Manifest::TomlLock,
        "package-lock.json" | "npm-shrinkwrap.json" => Manifest::NpmLock,
        "go.sum" => Manifest::GoSum,
        n if n.starts_with("requirements") && n.ends_with(".txt") => Manifest::Requirements,
        _ => return None,
    })
}

/// The dependency a manifest or lockfile line names, if any. `section` tracks the TOML section a
/// hunk is in (from the header lines it shows); a key in a hunk whose section is unknown counts
/// when its value is dependency-shaped (a quoted version or an inline table) — unknown leans
/// toward "new", never toward a waiver.
fn dependency_key(m: Manifest, line: &str, section: &mut Option<String>) -> Option<String> {
    let l = line.trim();
    if l.is_empty() || l.starts_with('#') {
        return None;
    }
    let unquote = |k: &str| k.trim().trim_matches('"').trim_matches('\'').to_string();
    match m {
        Manifest::Toml => {
            if let Some(h) = l.strip_prefix('[') {
                let h = h.trim_end_matches(']').trim_matches('[').trim();
                *section = Some(h.to_ascii_lowercase());
                // `[dependencies.foo]` names the dependency itself.
                return h
                    .rsplit_once("dependencies.")
                    .map(|(_, name)| unquote(name));
            }
            let (key, value) = l.split_once('=')?;
            let value = value.trim_start();
            let in_deps = section
                .as_deref()
                .is_some_and(|s| s.ends_with("dependencies"));
            let shaped = value.starts_with('{')
                || value
                    .strip_prefix('"')
                    .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit() || "^~=<>*".contains(c)));
            (in_deps || (section.is_none() && shaped)).then(|| unquote(key))
        }
        Manifest::PackageJson => {
            let (key, value) = l.split_once(':')?;
            let key = unquote(key);
            let value = value.trim().trim_end_matches(',').trim();
            let v = value.strip_prefix('"')?;
            let ranged = v.starts_with(|c: char| c.is_ascii_digit() || "^~=<>*".contains(c))
                || ["workspace:", "npm:", "file:", "link:", "git", "github:", "http"]
                    .iter()
                    .any(|p| v.starts_with(p));
            (ranged && key != "version" && key != "node" && !key.is_empty()).then_some(key)
        }
        Manifest::GoMod => {
            let l = l.strip_prefix("require").map_or(l, str::trim);
            let mut it = l.split_whitespace();
            let (module, version) = (it.next()?, it.next()?);
            (version.starts_with('v') && module.contains('.')).then(|| module.to_string())
        }
        Manifest::Requirements => {
            if l.starts_with('-') {
                return None;
            }
            let end = l
                .find(|c: char| "=<>~!;[ @".contains(c))
                .unwrap_or(l.len());
            Some(l[..end].to_ascii_lowercase()).filter(|k| !k.is_empty())
        }
        Manifest::TomlLock => l
            .strip_prefix("name")
            .map(str::trim_start)
            .and_then(|r| r.strip_prefix('='))
            .map(unquote),
        Manifest::NpmLock => l
            .strip_prefix("\"node_modules/")
            .and_then(|r| r.split_once('"'))
            .filter(|(_, rest)| rest.trim_start().starts_with(':'))
            .map(|(name, _)| name.rsplit("node_modules/").next().unwrap_or(name).to_string()),
        Manifest::GoSum => l.split_whitespace().next().map(str::to_string),
    }
}

/// A changed line carries a branch token ([`Thresholds::branch_tokens`]): a word token, or an
/// operator token anywhere in the line.
fn is_branch_line(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    THRESHOLDS.branch_tokens.iter().any(|tok| {
        if tok.chars().all(|c| c.is_ascii_alphabetic()) {
            l.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|w| w == *tok)
        } else {
            l.contains(tok)
        }
    })
}

/// The public or exported symbol a code line declares, by language: Rust `pub` items and fields
/// (never `pub(crate)`/`pub(super)`), JS/TS `export` declarations, top-level Python `def`/`class`
/// without a leading underscore, Go exported `func`/`type`.
fn public_decl(ext: Option<&str>, line: &str) -> Option<String> {
    let name_of = |w: &str| -> Option<String> {
        let n: String = w
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        (!n.is_empty()).then_some(n)
    };
    let mut words = line.split_whitespace();
    match ext? {
        "rs" => {
            if words.next()? != "pub" {
                return None;
            }
            let mut w = words.next()?;
            while matches!(w, "async" | "unsafe" | "extern" | "\"C\"") {
                w = words.next()?;
            }
            if w == "const" {
                // `pub const fn x` or `pub const X: T`.
                let next = words.next()?;
                return if next == "fn" { name_of(words.next()?) } else { name_of(next) };
            }
            match w {
                "fn" | "struct" | "enum" | "trait" | "type" | "static" | "mod" | "union" => {
                    name_of(words.next()?)
                }
                field if field.ends_with(':') => name_of(field).map(|f| format!(".{f}")),
                _ => None,
            }
        }
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => {
            if words.next()? != "export" {
                return None;
            }
            let mut w = words.next()?;
            while matches!(w, "async" | "declare" | "abstract") {
                w = words.next()?;
            }
            if w == "default" {
                return Some("default".to_string());
            }
            match w {
                "function" | "function*" | "class" | "const" | "let" | "var" | "interface"
                | "type" | "enum" | "namespace" => name_of(words.next()?.trim_start_matches('*')),
                _ => None,
            }
        }
        "py" => {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let mut w = words.next()?;
            if w == "async" {
                w = words.next()?;
            }
            matches!(w, "def" | "class")
                .then(|| name_of(words.next()?))
                .flatten()
                .filter(|n| !n.starts_with('_'))
        }
        "go" => {
            if line.starts_with(char::is_whitespace) {
                return None;
            }
            let w = words.next()?;
            let mut name = words.next()?;
            if w == "func" && name.starts_with('(') {
                // A method: skip the receiver.
                let rest = line.split_once(')')?.1;
                name = rest.split_whitespace().next()?;
            }
            matches!(w, "func" | "type")
                .then(|| name_of(name))
                .flatten()
                .filter(|n| n.starts_with(|c: char| c.is_ascii_uppercase()))
        }
        _ => None,
    }
}

/// The path on a `---`/`+++` line: git-unquoted, a trailing `\t<timestamp>` dropped from an
/// unquoted path, `/dev/null` as the empty (absent) side, and the `a/`/`b/` prefix removed.
fn header_path(raw: &str, prefix: &str) -> String {
    let p = if raw.starts_with('"') {
        git_unquote(raw)
    } else {
        raw.split('\t').next().unwrap_or("").to_string()
    };
    if p == "/dev/null" {
        return String::new();
    }
    p.strip_prefix(prefix).map_or(p.clone(), str::to_string)
}

/// Undo git's C-style path quoting (`"…"` with `\"`, `\\`, `\t`, `\n`, … and `\ooo` octal
/// bytes for non-ASCII under `core.quotePath`). An unquoted string is returned as is.
fn git_unquote(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else {
        return raw.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(7),
            Some(b'b') => bytes.push(8),
            Some(b'f') => bytes.push(12),
            Some(b'v') => bytes.push(11),
            Some(d @ b'0'..=b'7') => {
                let mut v = u32::from(d - b'0');
                for _ in 0..2 {
                    match it.peek() {
                        Some(n @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(n - b'0');
                            it.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(v as u8);
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// (WT-C3) The `kinds` token of one path in a plan context: `docs`, `test`, `config` or `code`
/// ([`classify`], the one classifier the score reads).
pub(crate) fn kind_name(path: &str) -> &'static str {
    match classify(path) {
        Kind::Docs => "docs",
        Kind::Test => "test",
        Kind::Config => "config",
        Kind::Code => "code",
    }
}

fn classify(path: &str) -> Kind {
    let t = &THRESHOLDS;
    let name = path.rsplit('/').next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let ext = ext.to_ascii_lowercase();
    let ext = ext.as_str();
    let in_docs_dir = || path.split('/').any(|c| t.docs_dirs.contains(&c));
    if t.docs_exts.contains(&ext) || (t.docs_dir_exts.contains(&ext) && in_docs_dir()) {
        Kind::Docs
    } else if path
        .split('/')
        .any(|c| matches!(c, "tests" | "test" | "__tests__"))
        || matches!(stem, "tests" | "test")
        || name.starts_with("test_")
        || [".test.", ".spec.", "_test."]
            .iter()
            .any(|m| name.contains(m))
    {
        Kind::Test
    } else if t.config_exts.contains(&ext) || path.split('/').any(|c| c.starts_with('.')) {
        Kind::Config
    } else {
        Kind::Code
    }
}

/// `crates/<name>` and `packages/<name>` are products; everything else is the root product.
fn product(path: &str) -> String {
    let c: Vec<&str> = path.split('/').collect();
    match c.as_slice() {
        [root @ ("crates" | "packages"), name, _, ..] => format!("{root}/{name}"),
        _ => ".".to_string(),
    }
}

/// A path token (split on `/ _ - .`) starts with one of `markers`.
fn has_token(path: &str, markers: &[&str]) -> bool {
    let path = path.to_ascii_lowercase();
    path.split(['/', '_', '-', '.'])
        .any(|tok| markers.iter().any(|m| tok.starts_with(m)))
        || markers.iter().any(|m| m.contains('_') && path.contains(m))
}

/// A lowercase substring match.
fn has_marker(text: &str, markers: &[&str]) -> bool {
    let text = text.to_ascii_lowercase();
    markers.iter().any(|m| text.contains(m))
}

fn is_type(k: &NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Enum
            | NodeKind::Interface
            | NodeKind::Trait
            | NodeKind::TypeAlias
    )
}

/// A test symbol: it lives in a test file, or its name says so.
fn is_test_symbol(n: &Node) -> bool {
    let name = n.name.to_ascii_lowercase();
    classify(&n.location.file) == Kind::Test
        || THRESHOLDS
            .test_name_prefixes
            .iter()
            .any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests;
