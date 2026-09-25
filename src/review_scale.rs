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
    /// The `a/` side, the path the graph indexed.
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
    /// G: the share of C that no test symbol reaches, 0..=1.
    pub test_gap: f32,
    pub critical: bool,
    pub destructive: bool,
    /// A traversal hit a cap, so `dependents` is a lower bound.
    pub truncated: bool,
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
    /// The model hook is consulted only from this deterministic score up.
    pub model_hook_min_score: u8,
    pub model_bonus_step: u8,
    pub model_bonus_max: u8,
    /// `(min score, plan)`, ascending.
    pub bands: &'static [(u8, ReviewPlan)],
    /// The floor of each band (DES-TEAMING-002 §8.5): one row per [`Thresholds::bands`] row, in
    /// the same order, so the monitor count, the floor and the high-risk rule are tuned together.
    pub floors: &'static [FloorRow],
    /// File extensions that are prose.
    pub docs_exts: &'static [&'static str],
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
    docs_exts: &["md", "mdx", "markdown", "rst", "adoc", "txt"],
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
};

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
    let mut s = ImpactSignals {
        critical: diff.critical,
        destructive: diff.destructive,
        ..Default::default()
    };
    if !diff.behavioural() {
        return Ok(s);
    }
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
    // Per seed, so the test gap is attributable. R is the union outside C.
    let spec = TraversalSpec::blast_radius(t.hops);
    let mut reached: BTreeSet<String> = BTreeSet::new();
    let mut untested = unindexed;
    for seed in &seeds {
        let sub = store.traverse(seed, &spec)?;
        s.truncated |= sub.truncated;
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
    s.test_gap = if s.changed_symbols == 0 {
        0.0
    } else {
        untested as f32 / s.changed_symbols as f32
    };
    Ok(s)
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
            "reach {reach}: {} changed symbol(s), {} dependent(s) within {} hops{}",
            s.changed_symbols,
            s.dependents,
            t.hops,
            if s.truncated { " (capped)" } else { "" }
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
            "test gap +{gap}: {:.0}% of changed symbols reached by no test",
            s.test_gap * 100.0
        ));
    }
    if s.critical {
        score += t.critical_points;
        reasons.push(format!(
            "critical +{}: a critical-path file changed",
            t.critical_points
        ));
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
    todo_floor(t, score, destructive)
}

fn todo_floor(_t: &Thresholds, _score: u8, _destructive: bool) -> Floor {
    Floor {
        band: String::new(),
        phases: Vec::new(),
        high_risk: false,
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
    let _ = (creator, touch);
    assess(&ChangeSignals::default(), graph, hook)
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
/// `rename from`/`rename to` (`copy from`/`copy to`) lines.
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
    let _ = paths;
    ChangeSignals::default()
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
            old.get_or_insert_with(|| git_unquote(p));
        } else if let Some(p) = l
            .strip_prefix("rename to ")
            .or_else(|| l.strip_prefix("copy to "))
        {
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
    let code: Vec<&str> = sides
        .iter()
        .copied()
        .filter(|p| !p.is_empty() && classify(p) != Kind::Docs)
        .collect();
    let k = match code.last() {
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
            old_path: old.clone(),
            old_lines: BTreeSet::new(),
        });
    }
    // Base-side line number of the next old line in the current hunk.
    let mut old_next: u32 = 0;
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
        // Context lines are scanned too (review on #600: a change that only loosens the guard
        // around an existing destructive call leaves the call itself on a context line).
        if behavioural && (added || removed || context) {
            let text = line.get(1..).unwrap_or("").to_ascii_lowercase();
            s.destructive |= t.destructive_line_markers.iter().any(|m| text.contains(m));
            if let Some(f) = s.touched.last_mut() {
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

fn classify(path: &str) -> Kind {
    let t = &THRESHOLDS;
    let name = path.rsplit('/').next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    if t.docs_exts.contains(&ext) {
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
