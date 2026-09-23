use super::*;

const DOCS_ONLY: &str = "\
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1,2 +1,3 @@
 # wicked-core
+Run `rm -rf target` to DROP TABLE the build cache.
 more
diff --git a/docs/guide.md b/docs/guide.md
--- a/docs/guide.md
+++ b/docs/guide.md
@@ -1 +1 @@
-old
+new
";

const SMALL_CODE: &str = "\
diff --git a/src/plan.rs b/src/plan.rs
--- a/src/plan.rs
+++ b/src/plan.rs
@@ -10,3 +10,4 @@
 fn plan() {
-    let x = 1;
+    let x = 2;
+    let y = x + 1;
 }
";

// The issue's example: a small change on a memory-erase path.
const MEMORY_ERASE: &str = "\
diff --git a/src/memory.rs b/src/memory.rs
--- a/src/memory.rs
+++ b/src/memory.rs
@@ -40,2 +40,3 @@
 fn retire(scope: &str) {
+    store.erase_scope(scope)?;
 }
";

fn big_cross_subsystem() -> String {
    let mut d = String::new();
    for f in [
        "src/distribute.rs",
        "src/pipeline.rs",
        "crates/wicked-council/src/lib.rs",
    ] {
        d.push_str(&format!(
            "diff --git a/{f} b/{f}\n--- a/{f}\n+++ b/{f}\n@@ -1 +1,200 @@\n"
        ));
        for i in 0..200 {
            d.push_str(&format!("+    let v{i} = {i};\n"));
        }
    }
    d
}

#[test]
fn diff_signals_classify_fixtures() {
    let docs = signals_from_diff(DOCS_ONLY);
    assert_eq!(
        docs,
        ChangeSignals {
            lines_added: 2,
            lines_removed: 1,
            docs_files: 2,
            ..Default::default()
        },
        "destructive words in prose are not a destructive path"
    );

    let small = signals_from_diff(SMALL_CODE);
    assert_eq!(
        small,
        ChangeSignals {
            lines_added: 2,
            lines_removed: 1,
            code_files: 1,
            subsystems: 1,
            ..Default::default()
        }
    );

    let erase = signals_from_diff(MEMORY_ERASE);
    assert_eq!((erase.lines_added, erase.code_files), (1, 1));
    assert!(erase.destructive && erase.critical, "{erase:?}");

    let big = signals_from_diff(&big_cross_subsystem());
    assert_eq!(
        (big.lines_added, big.code_files, big.subsystems),
        (600, 3, 3)
    );
    assert!(!big.destructive && !big.critical, "{big:?}");
}

#[test]
fn diff_signals_file_kinds_and_deletes() {
    let d = "\
diff --git a/tests/e2e.rs b/tests/e2e.rs
--- a/tests/e2e.rs
+++ b/tests/e2e.rs
@@ -1 +1 @@
-a
+b
diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1 +1 @@
-a
+b
diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
--- a/src/old.rs
+++ /dev/null
@@ -1 +0,0 @@
-fn gone() {}
";
    let s = signals_from_diff(d);
    assert_eq!(
        (s.test_files, s.config_files, s.code_files),
        (1, 1, 1),
        "{s:?}"
    );
    assert!(
        s.destructive,
        "deleting a code file is a destructive path: {s:?}"
    );
}

#[test]
fn policy_table() {
    let none = ReviewPlan {
        monitors: 0,
        depth: Depth::None,
        post_hoc_reviewer: false,
    };
    let cases: &[(&str, ChangeSignals, ReviewPlan)] = &[
        ("empty", ChangeSignals::default(), none),
        ("docs-only", signals_from_diff(DOCS_ONLY), none),
        (
            "small code",
            signals_from_diff(SMALL_CODE),
            ReviewPlan {
                monitors: 1,
                depth: Depth::Standard,
                post_hoc_reviewer: false,
            },
        ),
        (
            "small destructive (memory erase)",
            signals_from_diff(MEMORY_ERASE),
            ReviewPlan {
                monitors: 3,
                depth: Depth::Deep,
                post_hoc_reviewer: true,
            },
        ),
        (
            "destructive, non-critical path",
            ChangeSignals {
                lines_added: 3,
                code_files: 1,
                subsystems: 1,
                destructive: true,
                ..Default::default()
            },
            ReviewPlan {
                monitors: 2,
                depth: Depth::Deep,
                post_hoc_reviewer: true,
            },
        ),
        (
            "large cross-subsystem",
            signals_from_diff(&big_cross_subsystem()),
            ReviewPlan {
                monitors: 3,
                depth: Depth::Deep,
                post_hoc_reviewer: true,
            },
        ),
        (
            "large, one subsystem",
            ChangeSignals {
                lines_added: 500,
                code_files: 2,
                subsystems: 1,
                ..Default::default()
            },
            ReviewPlan {
                monitors: 2,
                depth: Depth::Standard,
                post_hoc_reviewer: true,
            },
        ),
        (
            "tests-only is behavioural",
            ChangeSignals {
                lines_added: 5,
                test_files: 1,
                subsystems: 1,
                ..Default::default()
            },
            ReviewPlan {
                monitors: 1,
                depth: Depth::Standard,
                post_hoc_reviewer: false,
            },
        ),
    ];
    for (name, signals, want) in cases {
        assert_eq!(review_plan(signals), *want, "{name}: {signals:?}");
    }
}

#[test]
fn most_review_is_the_ceiling() {
    let everything = ChangeSignals {
        lines_added: 5000,
        code_files: 40,
        subsystems: 9,
        critical: true,
        destructive: true,
        ..Default::default()
    };
    assert_eq!(review_plan(&everything).monitors, THRESHOLDS.max_monitors);
}

// Review on #600: only the `b/` side was classified, so moving code out of a critical subsystem
// into a docs path read as docs-only and summoned nothing (fail-open).
#[test]
fn code_to_docs_rename_is_behavioural_and_critical() {
    let d = "\
diff --git a/src/memory.rs b/docs/memory.md
similarity index 100%
rename from src/memory.rs
rename to docs/memory.md
";
    let s = signals_from_diff(d);
    assert_eq!((s.docs_files, s.code_files), (0, 1), "{s:?}");
    assert!(s.critical, "the a/ side is a critical subsystem: {s:?}");
    assert_eq!(
        review_plan(&s),
        ReviewPlan {
            monitors: 2,
            depth: Depth::Standard,
            post_hoc_reviewer: true
        }
    );
}

#[test]
fn deleted_critical_file_is_destructive_and_critical() {
    let d = "\
diff --git a/src/memory.rs b/src/memory.rs
deleted file mode 100644
--- a/src/memory.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn recall() {}
-fn store() {}
";
    let s = signals_from_diff(d);
    assert!(s.destructive && s.critical, "{s:?}");
    assert_eq!(
        review_plan(&s),
        ReviewPlan {
            monitors: 3,
            depth: Depth::Deep,
            post_hoc_reviewer: true
        }
    );
}

// Same fail-open class: a hunk with no `diff --git` header was counted as lines but no file, so
// it read as docs-only. An unattributed hunk counts as code.
#[test]
fn headerless_hunk_is_not_docs_only() {
    let s = signals_from_diff("@@ -1 +1 @@\n-a\n+b\n");
    assert_eq!(s.code_files, 1, "{s:?}");
    assert_eq!(review_plan(&s).monitors, 1);
}

// Review on #600: `remove_dir_all` and `remove_file` were listed but not `remove_dir`, so
// `std::fs::remove_dir(path)?;` in a small change got one standard monitor and no reviewer. The
// markers are now grouped by family; each family member has a realistic fixture line here, and the
// last assertion keeps the table and the fixtures in step.
#[test]
fn every_destructive_family_member_summons_deep_review() {
    let fixtures = [
        // Rust std::fs
        "std::fs::remove_dir(path)?;",
        "fs::remove_dir_all(&worktree)?;",
        "std::fs::remove_file(&lock)?;",
        // Node fs
        "await fs.rm(dir, { recursive: true, force: true });",
        "fs.rmSync(dir, { recursive: true });",
        "fs.rmdir(dir, cb);",
        "fs.unlink(file, cb);",
        "fs.unlinkSync(file);",
        "await rimraf(dist);",
        // Python
        "os.remove(path)",
        "os.unlink(path)",
        "os.rmdir(path)",
        "shutil.rmtree(tmp)",
        // shell
        "rm -r build",
        "rm -rf target",
        "rm -f state.db",
        "rmdir empty",
        // git
        "git clean -fdx",
        "git branch -D feat/old",
        "git push --force origin main",
        "git push -f origin main",
        "git reset --hard origin/main",
        "git worktree remove --force run-1",
        // SQL
        "DROP TABLE runs;",
        "ALTER TABLE runs DROP COLUMN cost;",
        "DROP INDEX idx_runs;",
        "DROP DATABASE wicked;",
        "DROP SCHEMA memory;",
        "TRUNCATE TABLE events;",
        "DELETE FROM memories WHERE scope = ?;",
        // generic verbs
        "store.erase_scope(scope)?;",
        "purge_expired(&conn)?;",
        "wipe_state_home()?;",
    ];
    let want = ReviewPlan {
        monitors: 2,
        depth: Depth::Deep,
        post_hoc_reviewer: true,
    };
    for line in fixtures {
        let d = format!(
            "diff --git a/src/plan.rs b/src/plan.rs\n--- a/src/plan.rs\n+++ b/src/plan.rs\n@@ -1 +1,2 @@\n fn f() {{}}\n+{line}\n"
        );
        let s = signals_from_diff(&d);
        assert!(s.destructive, "not destructive: {line:?} -> {s:?}");
        assert_eq!(review_plan(&s), want, "{line:?}");
    }
    let lower: Vec<String> = fixtures.iter().map(|l| l.to_ascii_lowercase()).collect();
    for m in THRESHOLDS.destructive_line_markers {
        assert!(
            lower.iter().any(|l| l.contains(m)),
            "marker {m:?} has no fixture line"
        );
    }
}

#[test]
fn destructive_words_in_a_docs_comment_stay_at_zero() {
    let d = "\
diff --git a/docs/ops.md b/docs/ops.md
--- a/docs/ops.md
+++ b/docs/ops.md
@@ -1 +1,2 @@
 # Ops
+<!-- the sweep calls remove_dir and git push --force; see the runbook -->
";
    let s = signals_from_diff(d);
    assert!(!s.destructive, "{s:?}");
    assert_eq!(
        review_plan(&s),
        ReviewPlan {
            monitors: 0,
            depth: Depth::None,
            post_hoc_reviewer: false
        }
    );
}
