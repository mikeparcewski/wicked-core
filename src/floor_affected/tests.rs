use super::*;
use wicked_apps_core::{
    Descriptor, Edge, EdgeKind, GraphWrite, Language, Location, ResolutionTier, Span, SqliteStore,
    Symbol,
};
use wicked_estate_core::{SymbolId, UnresolvedRef};

// ── fixtures: a three-member workspace and a real in-memory estate graph ─────────────────────

fn sym(name: &str) -> SymbolId {
    Symbol::global("test", None, vec![Descriptor::method(name, None)]).id()
}

/// `name` in `file`, 0-based lines `from..=to` (estate's spans).
fn node(name: &str, file: &str, from: u32, to: u32) -> Node {
    Node::new(
        sym(name),
        NodeKind::Function,
        name,
        Language::new("rust"),
        Location::new(
            file,
            Span {
                start_byte: 0,
                end_byte: 0,
                start_line: from,
                start_col: 0,
                end_line: to,
                end_col: 0,
            },
        ),
    )
}

const CORE: &str =
    "fn hot() -> u32 {\n    1\n}\n\n#[test]\nfn test_hot() {\n    assert_eq!(hot(), 1);\n}\n";
const OTHER: &str = "fn cold() -> u32 { 2 }\n#[test]\nfn test_cold() { assert_eq!(cold(), 2); }\n#[test]\nfn test_via_name() { assert_eq!(crate::core::hot(), 1); }\n";
const CALC: &str = "pub fn add(a: u32, b: u32) -> u32 { a + b }\n#[test]\nfn adds() { assert_eq!(add(1, 2), 3); }\n";
const FLOWS: &str =
    "#[test]\nfn reads_the_fixture() { let _ = include_str!(\"fixtures/data.json\"); }\n";

/// `test_hot` calls `hot` (resolved), `test_via_name` calls it by a name estate left
/// unresolved, `test_cold` calls only `cold`; the leaf crate's `adds` calls `add`.
fn graph() -> SqliteStore {
    graph_in(":memory:")
}

/// [`graph`] in the store at `db`.
fn graph_in(db: &str) -> SqliteStore {
    let mut g = wicked_apps_core::open_store(Some(db)).expect("a store");
    g.begin_batch().unwrap();
    g.upsert_nodes(&[
        node("hot", "src/core.rs", 0, 2),
        node("test_hot", "src/core.rs", 5, 7),
        node("cold", "src/other.rs", 0, 0),
        node("test_cold", "src/other.rs", 2, 2),
        node("test_via_name", "src/other.rs", 4, 4),
        node("add", "crates/leaf/src/calc.rs", 0, 0),
        node("adds", "crates/leaf/src/calc.rs", 2, 2),
        node("reads_the_fixture", "tests/flows.rs", 1, 1),
    ])
    .unwrap();
    let calls = |a: &str, b: &str| {
        Edge::new(
            sym(a),
            sym(b),
            EdgeKind::Calls,
            ResolutionTier::Parsed,
            "fixture",
        )
    };
    g.upsert_edges(&[
        calls("test_hot", "hot"),
        calls("test_cold", "cold"),
        calls("adds", "add"),
    ])
    .unwrap();
    g.upsert_unresolved_refs(&[UnresolvedRef::new(
        sym("test_via_name"),
        "hot",
        EdgeKind::Calls,
        Location::new(
            "src/other.rs",
            Span {
                start_byte: 0,
                end_byte: 0,
                start_line: 4,
                start_col: 0,
                end_line: 4,
                end_col: 0,
            },
        ),
    )])
    .unwrap();
    g.commit_batch().unwrap();
    g
}

fn target(kind: TargetKind, name: &str, src: &str) -> Target {
    Target {
        kind,
        name: name.into(),
        src: src.into(),
    }
}

/// `app` (the root: lib, bin, integration test `flows`) depends on `util`; `leaf` has no
/// dependent.
fn workspace() -> Workspace {
    Workspace {
        packages: vec![
            Package {
                name: "app".into(),
                dir: String::new(),
                targets: vec![
                    target(TargetKind::Lib, "app", "src/lib.rs"),
                    target(TargetKind::Bin, "app", "src/main.rs"),
                    target(TargetKind::Test, "flows", "tests/flows.rs"),
                ],
                member_deps: vec!["util".into()],
            },
            Package {
                name: "util".into(),
                dir: "crates/util".into(),
                targets: vec![target(TargetKind::Lib, "util", "crates/util/src/lib.rs")],
                member_deps: vec![],
            },
            Package {
                name: "leaf".into(),
                dir: "crates/leaf".into(),
                targets: vec![
                    target(TargetKind::Lib, "leaf", "crates/leaf/src/lib.rs"),
                    target(TargetKind::Test, "leaf_it", "crates/leaf/tests/leaf_it.rs"),
                ],
                member_deps: vec![],
            },
        ],
        outside_path_deps: vec![],
    }
}

fn files() -> Vec<String> {
    [
        "Cargo.toml",
        "Cargo.lock",
        "README.md",
        "src/lib.rs",
        "src/main.rs",
        "src/core.rs",
        "src/other.rs",
        "tests/flows.rs",
        "tests/fixtures/data.json",
        "tests/fixtures/unread.json",
        "crates/util/Cargo.toml",
        "crates/util/src/lib.rs",
        "crates/leaf/Cargo.toml",
        "crates/leaf/src/lib.rs",
        "crates/leaf/src/calc.rs",
        "crates/leaf/tests/leaf_it.rs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn text(f: &str) -> Option<String> {
    Some(
        match f {
            "src/core.rs" => CORE,
            "src/other.rs" => OTHER,
            "crates/leaf/src/calc.rs" => CALC,
            "tests/flows.rs" => FLOWS,
            "crates/leaf/tests/leaf_it.rs" => "#[test]\nfn it() {}\n",
            _ => "",
        }
        .to_string(),
    )
}

fn diff(file: &str, hunk: &str) -> String {
    format!("diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n{hunk}\n")
}

fn check() -> CargoTest {
    cargo_workspace_test(&s(&[
        "cargo",
        "test",
        "--locked",
        "--workspace",
        "--no-fail-fast",
    ]))
    .expect("core's own test check is a cargo workspace test")
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Run [`plan`] over the fixtures with `touched` / `diff`, a workspace and a graph that can each
/// be withheld.
fn run(touched: &[&str], diff: &str, ws: bool, graph_ok: bool, full_when: &[&str]) -> Plan {
    let g = graph();
    let w = workspace();
    let c = check();
    let touched = s(touched);
    let full_when = s(full_when);
    let files = files();
    let graph: Result<&dyn GraphRead, String> = if graph_ok {
        Ok(&g)
    } else {
        Err("graph indexed at 0000 is not the run base 1111".into())
    };
    plan(&Inputs {
        check: &c,
        touched: &touched,
        removed: &[],
        diff,
        workspace: if ws {
            Ok(&w)
        } else {
            Err("cargo metadata did not pass".into())
        },
        graph,
        base_text: &text,
        head_text: &text,
        files: &files,
        full_when: &full_when,
    })
}

fn script(p: &Plan) -> String {
    let argv = p.argv.as_ref().expect("an affected run");
    assert_eq!(&argv[..2], &s(&["sh", "-c"]));
    argv[2].clone()
}

// ── the selection ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_leaf_crate_change_selects_only_that_crates_tests() {
    let p = run(
        &["crates/leaf/src/calc.rs"],
        &diff("crates/leaf/src/calc.rs", "@@ -1 +1 @@"),
        true,
        true,
        &[],
    );
    assert_eq!(p.selection.mode, MODE_AFFECTED, "{:?}", p.selection);
    assert_eq!(p.selection.packages, s(&["leaf"]));
    assert_eq!(
        script(&p),
        "cargo test --locked --no-fail-fast -p leaf --lib -- adds"
    );
    assert_eq!(p.selection.tests, s(&["leaf: adds"]));
    assert_eq!(p.selection.changed_symbols, 1);
}

#[test]
fn a_changed_function_selects_the_tests_its_blast_radius_reaches_and_no_other() {
    let p = run(
        &["src/core.rs"],
        &diff("src/core.rs", "@@ -2 +2 @@"),
        true,
        true,
        &[],
    );
    assert_eq!(p.selection.mode, MODE_AFFECTED, "{:?}", p.selection);
    // `test_hot` by its resolved call (and as a test of the touched file), `test_via_name` by
    // the call estate left unresolved; never `test_cold`, `util` or `leaf`.
    assert_eq!(
        script(&p),
        "cargo test --locked --no-fail-fast -p app --lib -- test_hot test_via_name"
    );
    assert_eq!(p.selection.packages, s(&["app"]));
    assert!(p
        .selection
        .note()
        .starts_with("affected set: 1 invocation(s), 2 named lib test(s)"));
}

#[test]
fn shared_infrastructure_runs_the_full_suite_and_says_why() {
    for f in [
        "Cargo.lock",
        "crates/util/Cargo.toml",
        "build.rs",
        ".cargo/config.toml",
    ] {
        let p = run(&[f, "src/core.rs"], "", true, true, &[]);
        assert_eq!(p.selection.mode, MODE_FULL, "{f}");
        assert!(p.argv.is_none());
        assert!(
            p.selection.reason.contains(f),
            "{f}: {}",
            p.selection.reason
        );
    }
    // A crate root carries the module tree.
    let p = run(&["src/lib.rs"], "", true, true, &[]);
    assert!(
        p.selection.reason.contains("crate root"),
        "{:?}",
        p.selection
    );
    // And whatever the base's widening list names (core lists its actor).
    let p = run(&["src/core.rs"], "", true, true, &["src/core.rs"]);
    assert_eq!(p.selection.mode, MODE_FULL);
    assert!(
        p.selection.reason.contains(AFFECTED_CONFIG),
        "{:?}",
        p.selection
    );
}

#[test]
fn without_a_usable_graph_or_workspace_the_full_suite_runs() {
    let d = diff("src/core.rs", "@@ -2 +2 @@");
    let p = run(&["src/core.rs"], &d, true, false, &[]);
    assert_eq!(p.selection.mode, MODE_FULL);
    assert!(
        p.selection.reason.starts_with("no code graph at the base"),
        "{:?}",
        p.selection
    );
    let p = run(&["src/core.rs"], &d, false, true, &[]);
    assert_eq!(p.selection.mode, MODE_FULL);
    assert!(
        p.selection.reason.contains("workspace"),
        "{:?}",
        p.selection
    );
}

#[test]
fn a_line_outside_every_symbol_and_an_unnamed_data_file_run_the_full_suite() {
    let g = graph();
    let w = workspace();
    let c = check();
    let files = files();
    let base = |f: &str| {
        (f == "src/core.rs")
            .then(|| format!("{CORE}macro_rules! m {{ () => {{}} }}\n"))
            .or_else(|| text(f))
    };
    let touched = s(&["src/core.rs"]);
    let d = diff("src/core.rs", "@@ -9 +9 @@");
    let p = plan(&Inputs {
        check: &c,
        touched: &touched,
        removed: &[],
        diff: &d,
        workspace: Ok(&w),
        graph: Ok(&g),
        base_text: &base,
        head_text: &text,
        files: &files,
        full_when: &[],
    });
    assert!(
        p.selection.reason.contains("line 9 of `src/core.rs`"),
        "{:?}",
        p.selection
    );
    // A blank or comment line is not: it changes no behaviour.
    let p = run(
        &["src/core.rs"],
        &diff("src/core.rs", "@@ -4 +4 @@"),
        true,
        true,
        &[],
    );
    assert_eq!(p.selection.mode, MODE_AFFECTED, "{:?}", p.selection);
    // A data file nothing names cannot be traced; one a test names selects that test's target.
    let p = run(&["tests/fixtures/unread.json"], "", true, true, &[]);
    assert!(
        p.selection.reason.contains("no source names"),
        "{:?}",
        p.selection
    );
    let p = run(&["tests/fixtures/data.json"], "", true, true, &[]);
    assert_eq!(
        script(&p),
        "cargo test --locked --no-fail-fast -p app --test flows"
    );
}

/// (core#1187, proof on core 3cd2463) Core's changes touch `CHANGELOG.md`, and a lift test that
/// writes its OWN `CHANGELOG.md` is documented above its function, outside every span: a comment
/// reads no file, so it never forces the full suite. A doc comment can hold a doc test, so it runs
/// that package's doc tests; a plain comment alone does not count as reading the file.
#[test]
fn a_comment_that_names_a_touched_file_is_not_a_read_of_it() {
    let g = graph();
    let w = workspace();
    let c = check();
    let files = files();
    let other = |f: &str| {
        (f == "src/other.rs")
            .then(|| format!("/// Writes its own CHANGELOG.md.\n// and data.json\n{OTHER}"))
            .or_else(|| text(f))
    };
    let touched = s(&["CHANGELOG.md", "src/core.rs"]);
    let d = diff("src/core.rs", "@@ -2 +2 @@");
    let p = plan(&Inputs {
        check: &c,
        touched: &touched,
        removed: &[],
        diff: &d,
        workspace: Ok(&w),
        graph: Ok(&g),
        base_text: &other,
        head_text: &other,
        files: &files,
        full_when: &[],
    });
    assert_eq!(p.selection.mode, MODE_AFFECTED, "{:?}", p.selection);
    assert_eq!(
        script(&p),
        "cargo test --locked --no-fail-fast -p app --lib -- test_hot test_via_name \
         && cargo test --locked --no-fail-fast -p app --doc"
    );
    // A data file only a plain comment names is still untraced.
    let touched = s(&["tests/fixtures/unread.json"]);
    let p = plan(&Inputs {
        check: &c,
        touched: &touched,
        removed: &[],
        diff: "",
        workspace: Ok(&w),
        graph: Ok(&g),
        base_text: &|f: &str| {
            (f == "src/other.rs")
                .then(|| format!("// unread.json\n{OTHER}"))
                .or_else(|| text(f))
        },
        head_text: &|f: &str| {
            (f == "src/other.rs")
                .then(|| format!("// unread.json\n{OTHER}"))
                .or_else(|| text(f))
        },
        files: &files,
        full_when: &[],
    });
    assert!(
        p.selection.reason.contains("no source names"),
        "{:?}",
        p.selection
    );
}

#[test]
fn a_selection_that_names_no_test_and_a_deletion_run_the_full_suite() {
    // Documentation alone reaches no test: the check never passes by running nothing.
    let p = run(&["README.md"], "", true, true, &[]);
    assert_eq!(p.selection.mode, MODE_FULL);
    assert!(
        p.selection.reason.contains("running nothing"),
        "{:?}",
        p.selection
    );
    let g = graph();
    let w = workspace();
    let c = check();
    let files = files();
    let p = plan(&Inputs {
        check: &c,
        touched: &s(&["src/core.rs"]),
        removed: &s(&["src/gone.rs"]),
        diff: "",
        workspace: Ok(&w),
        graph: Ok(&g),
        base_text: &text,
        head_text: &text,
        files: &files,
        full_when: &[],
    });
    assert!(
        p.selection.reason.contains("src/gone.rs"),
        "{:?}",
        p.selection
    );
}

#[test]
fn a_binary_or_an_integration_test_runs_as_its_own_target() {
    let p = run(
        &["tests/flows.rs"],
        &diff("tests/flows.rs", "@@ -2 +2 @@"),
        true,
        true,
        &[],
    );
    assert_eq!(
        script(&p),
        "cargo test --locked --no-fail-fast -p app --test flows"
    );
}

// ── the parts ─────────────────────────────────────────────────────────────────────────────────

#[test]
fn only_a_whole_workspace_cargo_test_with_carried_options_is_narrowed() {
    let c = cargo_workspace_test(&s(&[
        "cargo",
        "test",
        "-j",
        "4",
        "--workspace",
        "--",
        "--test-threads=2",
    ]))
    .unwrap();
    assert_eq!(c.opts, s(&["-j", "4"]));
    assert_eq!(c.test_args, s(&["--test-threads=2"]));
    for argv in [
        &["cargo", "test", "--locked"][..],
        &["cargo", "test", "--workspace", "-p", "x"],
        &["cargo", "test", "--workspace", "--features", "postgres"],
        &["cargo", "nextest", "run", "--workspace"],
        &["npm", "test"],
    ] {
        assert!(cargo_workspace_test(&s(argv)).is_none(), "{argv:?}");
    }
}

#[test]
fn the_diff_names_removed_lines_and_both_sides_of_an_insertion() {
    let d = "--- a/x.rs\n+++ b/x.rs\n@@ -3,2 +3,0 @@\n-a\n-b\n@@ -10,0 +9,4 @@\n+c\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n";
    let o = old_lines(d);
    assert_eq!(o.len(), 1, "a new file has no base side");
    assert_eq!(o["x.rs"], [3, 4, 10, 11].into_iter().collect());
}

#[test]
fn test_functions_are_found_by_their_attribute() {
    let src = "#[test]\nfn a() {}\n#[tokio::test]\n#[ignore]\nasync fn b() {}\nfn helper() {}\n/// doc\n#[cfg(test)]\nfn not_a_test() {}\n#[test] fn c() {}\n";
    let names: Vec<String> = test_fns(src).into_iter().map(|(_, n)| n).collect();
    assert_eq!(names, s(&["a", "b", "c"]));
}

#[test]
fn cargo_metadata_is_read_into_members_targets_and_path_dependencies() {
    let json = serde_json::json!({
        "workspace_root": "/w",
        "workspace_members": ["app 0.1.0 (path+file:///w)", "util 0.1.0 (path+file:///w/crates/util)"],
        "packages": [
            {"id": "app 0.1.0 (path+file:///w)", "name": "app", "manifest_path": "/w/Cargo.toml",
             "targets": [
                {"kind": ["lib"], "name": "app", "src_path": "/w/src/lib.rs"},
                {"kind": ["test"], "name": "flows", "src_path": "/w/tests/flows.rs"}],
             "dependencies": [{"name": "util", "path": "/w/crates/util"},
                              {"name": "ext", "path": "/w/vendor/ext"},
                              {"name": "serde"}]},
            {"id": "util 0.1.0 (path+file:///w/crates/util)", "name": "util",
             "manifest_path": "/w/crates/util/Cargo.toml",
             "targets": [{"kind": ["lib"], "name": "util", "src_path": "/w/crates/util/src/lib.rs"}],
             "dependencies": []}
        ]
    })
    .to_string();
    let ws = parse_metadata(&json).unwrap();
    assert_eq!(ws.packages.len(), 2);
    assert_eq!(ws.packages[0].member_deps, s(&["util"]));
    assert_eq!(ws.packages[1].dir, "crates/util");
    assert_eq!(ws.packages[0].targets[1].src, "tests/flows.rs");
    assert_eq!(ws.outside_path_deps, s(&["vendor/ext"]));
    let closure = ws.reverse_closure(&["util".to_string()].into_iter().collect());
    assert_eq!(
        closure,
        ["app".to_string(), "util".to_string()]
            .into_iter()
            .collect()
    );
}

// ── a real tree (core#1187): git, the base's widening list, the graph's age ───────────────────

#[cfg(unix)]
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The selection reads the change from git against the base, the widening list from the BASE tree
/// only (a change cannot narrow its own floor, it can only make the full suite run), and only a
/// graph indexed AT the base: a stale graph runs the full suite.
#[cfg(unix)]
#[test]
fn a_real_tree_is_read_against_its_base_and_a_graph_at_another_commit_runs_the_full_suite() {
    let dir = std::env::temp_dir().join(format!(
        "wc-affected-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for f in files() {
        let p = dir.join(&f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text(&f).unwrap()).unwrap();
    }
    git_in(&dir, &["init", "-q"]);
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-qm", "base"]);
    let base = git_in(&dir, &["rev-parse", "HEAD"]);
    let db = dir.join("graph.db");
    let mut g = graph_in(&db.to_string_lossy());
    let stamp = |g: &mut SqliteStore, commit: &str| {
        g.set_repo_info(&wicked_estate_core::RepoInfo {
            commit: Some(commit.to_string()),
            ..Default::default()
        })
        .unwrap();
    };
    stamp(&mut g, &base);
    // The change: `hot`'s body (line 2), and a head-side widening list that tries to matter.
    std::fs::write(
        dir.join("src/core.rs"),
        CORE.replace("    1\n", "    1 + 0\n"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".wicked")).unwrap();
    std::fs::write(dir.join(AFFECTED_CONFIG), r#"{"full": ["src/core.rs"]}"#).unwrap();
    let git_dir = dir.join(".git");
    let c = check();
    let w = workspace();
    let plan_now = |touched: &[&str]| {
        plan_for_tree(
            &Tree {
                worktree: &dir,
                git_dir: &git_dir,
                base: &base,
                touched: &s(touched),
                removed: &[],
                graph: Some(&db),
            },
            &c,
            &mut || Ok(w.clone()),
        )
    };
    let p = plan_now(&["src/core.rs"]);
    assert_eq!(p.selection.mode, MODE_AFFECTED, "{:?}", p.selection);
    assert_eq!(
        script(&p),
        "cargo test --locked --no-fail-fast -p app --lib -- test_hot test_via_name"
    );
    // The BASE's widening list runs the full suite.
    git_in(&dir, &["add", AFFECTED_CONFIG]);
    git_in(&dir, &["commit", "-qm", "widen"]);
    let widened = git_in(&dir, &["rev-parse", "HEAD"]);
    stamp(&mut g, &widened);
    let p = plan_for_tree(
        &Tree {
            worktree: &dir,
            git_dir: &git_dir,
            base: &widened,
            touched: &s(&["src/core.rs"]),
            removed: &[],
            graph: Some(&db),
        },
        &c,
        &mut || Ok(w.clone()),
    );
    assert_eq!(p.selection.mode, MODE_FULL, "{:?}", p.selection);
    assert!(
        p.selection.reason.contains(AFFECTED_CONFIG),
        "{:?}",
        p.selection
    );
    // A graph indexed at another commit (here: the widened one, read against the first base).
    let p = plan_now(&["src/core.rs"]);
    assert_eq!(p.selection.mode, MODE_FULL, "{:?}", p.selection);
    assert!(
        p.selection.reason.starts_with("no code graph at the base"),
        "{:?}",
        p.selection
    );
    let _ = std::fs::remove_dir_all(&dir);
}
