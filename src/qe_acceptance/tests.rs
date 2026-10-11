use super::*;
use crate::assurance::{QeOverride, RunAssurance};
use crate::domain::{HumanConfirm, SessionStatus};
use crate::scope::EntityMode;
use crate::workflow::PhaseRole;
use wicked_apps_core::spawn::HardenedCommand;

fn session(assurance: RunAssurance) -> AgentSession {
    AgentSession {
        intent_amendments: Vec::new(),
        id: "s".into(),
        workflow_id: "wf-s".into(),
        problem: "p".into(),
        entity_mode: EntityMode::Shared,
        collection_scope: None,
        clis: vec![],
        status: SessionStatus::Executing,
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        unit_ix: 0,
        attempt: 0,
        workdir: None,
        repo_ref: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        project_id: None,
        archived_at: None,
        archive_note: None,
        verified_tree: None,
        run_branch: None,
        base_commit: None,
        finished_at: None,
        benched_seats: Vec::new(),
        team: None,
        team_plan: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        assurance,
    }
}

fn requiring() -> RunAssurance {
    let declared = vec![
        a::DISTINCT_EVALUATOR.to_string(),
        a::JUDGE.to_string(),
        a::QE_ACCEPTANCE.to_string(),
    ];
    RunAssurance::new(Some(&declared), false)
}

fn qe_unit(ord: u32) -> WorkUnit {
    let mut u = WorkUnit::pending(format!("s:test{ord}"), "s", ord, "verify");
    u.repo_checks_floor = true;
    u.role = PhaseRole::Evaluator;
    u
}

fn creator(ord: u32) -> WorkUnit {
    let mut u = WorkUnit::pending(format!("s:build{ord}"), "s", ord, "build");
    u.role = PhaseRole::Creator;
    u.executes_code = true;
    u
}

fn waived_at(ord: u32) -> QeAcceptance {
    QeAcceptance {
        status: a::QE_WAIVED.into(),
        basis: a::QE_BASIS_DIFF.into(),
        score: Some(20),
        threshold: 20,
        reason: "waived: impact score 20".into(),
        reasons: vec!["reach 20: 1 changed symbol(s)".into()],
        ord: Some(ord),
        tree: Some("t1".into()),
    }
}

#[test]
fn a_run_that_does_not_require_qe_acceptance_is_never_decided() {
    let s = session(RunAssurance::default());
    assert!(matches!(
        on_dispatch(&s, &qe_unit(3), None, None),
        Dispatched::Unchanged
    ));
}

#[test]
fn an_operator_decision_is_never_rescored_or_revoked() {
    for over in [QeOverride::Skip("hotfix".into()), QeOverride::Force] {
        let mut s = session(requiring().with_qe_override(&over).unwrap());
        s.base_commit = Some("b".into());
        for u in [qe_unit(3), creator(2)] {
            assert!(
                matches!(on_dispatch(&s, &u, None, None), Dispatched::Unchanged),
                "{over:?}"
            );
        }
    }
}

#[test]
fn the_qe_unit_with_no_repo_diff_is_required_with_the_reason() {
    let s = session(requiring());
    let Dispatched::Decided(d) = on_dispatch(&s, &qe_unit(3), None, None) else {
        panic!("decided");
    };
    assert_eq!((d.status.as_str(), d.basis.as_str()), ("required", "diff"));
    assert_eq!(d.ord, Some(3));
    assert!(d.reason.contains("no repository diff to score"), "{d:?}");
    // The same dispatch again changes nothing.
    let mut s2 = s.clone();
    s2.assurance.qe = Some(d);
    assert!(matches!(
        on_dispatch(&s2, &qe_unit(3), None, None),
        Dispatched::Unchanged
    ));
}

#[test]
fn a_creator_after_a_waiver_revokes_it() {
    let mut s = session(requiring());
    s.assurance.qe = Some(waived_at(4));
    let Dispatched::Decided(d) = on_dispatch(&s, &creator(5), None, None) else {
        panic!("revoked");
    };
    assert_eq!(d.status, "required");
    assert!(
        d.reason
            .contains("creator unit 5 runs after the waiver at unit 4"),
        "{d:?}"
    );
    // A non-creator after the waiver (a review, the deliver tool) leaves it standing.
    let mut review = WorkUnit::pending("s:review", "s", 5, "review");
    review.role = PhaseRole::Evaluator;
    assert!(matches!(
        on_dispatch(&s, &review, None, None),
        Dispatched::Unchanged
    ));
}

/// A real repository: the diff is the base commit against the QE unit's starting tree, and a
/// file with one commit of history is low history.
#[test]
fn the_run_diff_and_history_come_from_the_repository() {
    let dir = std::env::temp_dir().join(format!("wicked-qe-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .hardened()
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("src/a.rs"), "fn a() {\n    1\n}\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "base"]);
    let base = git(&["rev-parse", "HEAD"]);
    std::fs::write(dir.join("src/a.rs"), "fn a() {\n    2\n}\n").unwrap();
    std::fs::write(dir.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    git(&["add", "-A"]);
    let tree = git(&["write-tree"]);
    let git_dir = dir.join(".git");
    let diff = run_diff(&dir, &git_dir, &base, &tree).unwrap();
    let mut signals = rs::signals_from_diff(&diff);
    assert_eq!(signals.code_files, 2, "{diff}");
    assert_eq!(
        signals.new_public_symbols,
        BTreeSet::from(["b".to_string()]),
        "{diff}"
    );
    with_history(&mut signals, &dir, &git_dir, &base);
    assert_eq!(
        signals.low_history, 1,
        "src/a.rs has one commit; src/b.rs is new"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_decision_words_name_the_score_and_the_line() {
    let a_ = rs::Assessment {
        deterministic: 20,
        score: 20,
        reasons: vec!["reach 20: 1 changed symbol(s), 0 dependent(s) within 3 hops".into()],
        model: None,
        signals: Some(rs::ImpactSignals {
            changed_symbols: 1,
            products: 1,
            ..Default::default()
        }),
        plan: rs::plan_for(20),
    };
    let d = from_assessment(&a_, 4, "t1");
    assert_eq!(
        (d.status.as_str(), d.score, d.threshold),
        ("waived", Some(20), 20)
    );
    assert!(
        d.reason
            .starts_with("waived: impact score 20 at or below the waiver line 20"),
        "{d:?}"
    );
    let a_ = rs::Assessment {
        score: 30,
        deterministic: 30,
        signals: Some(rs::ImpactSignals {
            changed_symbols: 1,
            products: 1,
            unindexed: 1,
            ..Default::default()
        }),
        ..a_
    };
    let d = from_assessment(&a_, 4, "t1");
    assert_eq!(d.status, "required");
    assert!(
        d.reason
            .starts_with("required: impact score 30 above the waiver line 20"),
        "{d:?}"
    );
}

/// wicked-crew#951: the run's QE ledger is `<evidence root>/.wicked-qe` — outside every worktree —
/// and a run without an evidence root (or a blank one) has none; only a unit carrying the root is
/// handed `WICKED_QE_LEDGER_DIR`.
#[test]
fn the_run_qe_ledger_root_is_the_evidence_roots_ledger_and_rides_only_its_unit() {
    let mut s = session(requiring());
    assert_eq!(ledger_root(&s), None, "no evidence root, no QE ledger root");
    s.evidence_root = Some("  ".into());
    assert_eq!(ledger_root(&s), None, "a blank evidence root is none");
    s.evidence_root = Some("/home/u/.wicked/walkthroughs/run-1".into());
    assert_eq!(
        ledger_root(&s),
        Some(Path::new("/home/u/.wicked/walkthroughs/run-1").join(".wicked-qe"))
    );
    let mut u = qe_unit(4);
    assert!(ledger_env(&u).is_empty(), "no root, no variable");
    u.qe_ledger_root = Some("/e/.wicked-qe".into());
    assert_eq!(
        ledger_env(&u),
        vec![(
            "WICKED_QE_LEDGER_DIR".to_string(),
            "/e/.wicked-qe".to_string()
        )]
    );
}

#[test]
fn the_prompt_directive_says_run_or_do_not_run() {
    let mut q = waived_at(4);
    assert!(directive(&q, "wicked-garden:qe").contains("do not run the acceptance pipeline"));
    q.status = a::QE_REQUIRED.into();
    let line = directive(&q, "wicked-garden:qe");
    assert!(
        line.contains("\"wicked-garden:qe\" accept") && line.contains("WICKED_RUN_ID"),
        "{line}"
    );
    // (wicked-crew#951) …and names the ledger root it is handed, outside the worktree.
    assert!(
        line.contains("$WICKED_QE_LEDGER_DIR") && line.contains("outside the worktree"),
        "{line}"
    );
    q.status = a::QE_SKIPPED.into();
    assert!(directive(&q, "x").contains("skipped by the operator"));
}

/// A temp repository with `src/a.rs` committed `commits` times (`fn a` on lines 1-3), the run base
/// at its head, and a graph indexed AT the base holding `fn a`.
struct Repo {
    dir: std::path::PathBuf,
    base: String,
}

impl Repo {
    fn new(name: &str, commits: u32) -> Self {
        let dir = std::env::temp_dir().join(format!("wicked-qe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let mut r = Self {
            dir,
            base: String::new(),
        };
        r.git(&["init", "-q"]);
        for i in 0..commits {
            std::fs::write(r.dir.join("src/a.rs"), format!("fn a() {{\n    {i}\n}}\n")).unwrap();
            r.git(&["add", "-A"]);
            r.git(&["commit", "-q", "-m", &format!("c{i}")]);
        }
        r.base = r.git(&["rev-parse", "HEAD"]);
        r
    }
    fn git(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .hardened()
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    /// Write the change, stage it, and return the tree the QE unit would start on.
    fn change(&self, files: &[(&str, &str)]) -> String {
        for (p, text) in files {
            let path = self.dir.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        self.git(&["add", "-A"]);
        self.git(&["write-tree"])
    }
    fn graph(&self) -> wicked_apps_core::SqliteStore {
        use wicked_apps_core::{
            Descriptor, GraphWrite, Language, Location, Node, NodeKind, Span, Symbol,
        };
        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        let node = Node::new(
            Symbol::global("test", None, vec![Descriptor::method("a", None)]).id(),
            NodeKind::Function,
            "a",
            Language::new("rust"),
            Location::new(
                "src/a.rs",
                Span {
                    start_byte: 0,
                    end_byte: 0,
                    start_line: 1,
                    start_col: 0,
                    end_line: 3,
                    end_col: 0,
                },
            ),
        );
        store.begin_batch().unwrap();
        store.upsert_nodes(&[node]).unwrap();
        store.commit_batch().unwrap();
        store
            .set_repo_info(&wicked_estate_core::RepoInfo {
                commit: Some(self.base.clone()),
                ..Default::default()
            })
            .unwrap();
        store
    }
    fn decide(&self, tree: &str) -> QeAcceptance {
        let mut s = session(requiring());
        s.base_commit = Some(self.base.clone());
        let store = self.graph();
        let git_dir = self.dir.join(".git");
        decide_with(
            &s,
            4,
            Some(&self.dir),
            Some((tree, &git_dir)),
            |signals, _, base| {
                rs::assess(
                    signals,
                    Graph::Ready {
                        store: &store,
                        base_commit: base,
                    },
                    None,
                )
            },
        )
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// THE low-score proof: a one-line edit inside a leaf function with history, on a graph indexed at
/// the base, is waived — and the decision names the score and the line.
#[test]
fn a_one_line_leaf_edit_with_history_is_waived_with_its_reason() {
    let repo = Repo::new("waive", 3);
    let tree = repo.change(&[("src/a.rs", "fn a() {\n    42\n}\n")]);
    let d = repo.decide(&tree);
    assert_eq!((d.status.as_str(), d.score), ("waived", Some(20)), "{d:?}");
    assert!(
        d.reason
            .starts_with("waived: impact score 20 at or below the waiver line 20"),
        "{d:?}"
    );
    assert_eq!(d.tree.as_deref(), Some(tree.as_str()));
}

/// The same edit on a file with one commit of history is novelty, so required.
#[test]
fn the_same_edit_on_a_first_touch_file_is_required() {
    let repo = Repo::new("history", 1);
    let tree = repo.change(&[("src/a.rs", "fn a() {\n    42\n}\n")]);
    let d = repo.decide(&tree);
    assert_eq!(d.status, "required", "{d:?}");
    assert!(
        d.reason.contains("touched path(s) with under 3 commits"),
        "{d:?}"
    );
}

/// A brand-new file that adds a dependency, with nothing depending on it: required.
#[test]
fn a_new_file_with_a_new_dependency_is_required_end_to_end() {
    let repo = Repo::new("newdep", 3);
    let tree = repo.change(&[
        ("src/b.rs", "fn b() {}\n"),
        (
            "Cargo.toml",
            "[package]\nname = \"x\"\n\n[dependencies]\nserde_json = \"1\"\n",
        ),
    ]);
    let d = repo.decide(&tree);
    assert_eq!(d.status, "required", "{d:?}");
    assert!(
        d.reason.contains("new dependency") && d.reason.contains("new or unindexed file"),
        "{d:?}"
    );
}

/// codex r1: every unit that may change the scored tree revokes a waiver — a `produce` creator
/// (no `executes_code`), a creator Tool step, a neutral `executes_code` unit — but not the deliver
/// step.
#[test]
fn any_tree_changing_unit_after_a_waiver_revokes_it() {
    let mut s = session(requiring());
    s.assurance.qe = Some(waived_at(4));
    let mut produce = WorkUnit::pending("s:produce", "s", 5, "produce");
    produce.role = PhaseRole::Creator;
    let mut tool = WorkUnit::pending("s:run", "s", 5, "run");
    tool.role = PhaseRole::Creator;
    tool.tool_cmd = Some(vec!["make".into()]);
    let mut cutover = WorkUnit::pending("s:cutover", "s", 5, "cutover");
    cutover.executes_code = true;
    for u in [produce, tool, cutover] {
        assert!(
            matches!(on_dispatch(&s, &u, None, None), Dispatched::Decided(ref d) if d.status == "required"),
            "{}",
            u.id
        );
    }
    let mut deliver = WorkUnit::pending("s:deliver", "s", 6, "deliver");
    deliver.role = PhaseRole::Creator;
    deliver.tool_cmd = Some(vec!["deliver".into()]);
    assert!(matches!(
        on_dispatch(&s, &deliver, None, None),
        Dispatched::Unchanged
    ));
}

/// codex r1: a QE dispatch that carries a floor fix (a seat changes the tree after the score) is
/// required whatever the diff scores.
#[test]
fn a_qe_dispatch_carrying_a_floor_fix_is_required() {
    let repo = Repo::new("floorfix", 3);
    let tree = repo.change(&[("src/a.rs", "fn a() {\n    42\n}\n")]);
    assert_eq!(
        repo.decide(&tree).status,
        "waived",
        "the tree alone would be waived"
    );
    let mut s = session(requiring());
    s.base_commit = Some(repo.base.clone());
    let mut u = qe_unit(4);
    u.repo_checks = Some(
        serde_json::from_value(serde_json::json!({
            "detected": [], "checks": [], "skipped": [], "passed": false,
            "requested_rerun": {"mode": "floor_fix", "output": "", "fix": {"note": "fix the test", "seat": "codex"}}
        }))
        .expect("a repo-checks report with a requested fix"),
    );
    let git_dir = repo.dir.join(".git");
    let Dispatched::Decided(d) = on_dispatch(&s, &u, Some(&repo.dir), Some((&tree, &git_dir)))
    else {
        panic!("decided");
    };
    assert_eq!(d.status, "required");
    assert!(d.reason.contains("floor fix"), "{d:?}");
}

/// codex r3: what a hunk's context window cannot see, the whole files can — a member added far
/// below its group's opener, and an import group made public, are new public symbols.
#[test]
fn whole_files_see_what_the_hunk_window_cannot() {
    let mut repo = Repo::new("surface", 3);
    let mut base_lib = String::from("pub use inner::{\n    A,\n");
    for i in 0..10 {
        base_lib.push_str(&format!("    M{i},\n"));
    }
    base_lib.push_str("};\nuse other::{\n    X,\n};\n");
    std::fs::write(repo.dir.join("src/lib.rs"), &base_lib).unwrap();
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "lib"]);
    repo.base = repo.git(&["rev-parse", "HEAD"]);
    // A member appended far below the opener, and the private group made public.
    let head_lib = base_lib
        .replace("    M9,\n};", "    M9,\n    Z,\n};")
        .replace("use other::{", "pub use other::{");
    let tree = repo.change(&[("src/lib.rs", &head_lib)]);
    let git_dir = repo.dir.join(".git");
    let diff = run_diff(&repo.dir, &git_dir, &repo.base, &tree).unwrap();
    let mut signals = rs::signals_from_diff(&diff);
    with_whole_files(&mut signals, &repo.dir, &git_dir, &repo.base, &tree);
    assert!(
        signals.new_public_symbols.contains("use inner::Z")
            && signals.new_public_symbols.contains("use other::X"),
        "{:?}",
        signals.new_public_symbols
    );
}
