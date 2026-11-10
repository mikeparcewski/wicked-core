//! How long one `wicked-estate index` of a tree may run (wicked-crew#1481, operator ruling
//! 2026-11-10): sized from the code, never a fixed bound.
//!
//! `budget = max(120 s, files × 0.30 s) × 1.15`
//!
//! - **files**: the files `git ls-files` lists for the tree (tracked, plus untracked files that are
//!   not ignored: a checkout index reads those too, so the count is never smaller than what is read).
//!   Cheap and deterministic; bytes over-count lockfiles and assets.
//! - **0.30 s per file**: the worst measured rate on a loaded host (0.24 s, wicked-crew 1,291 files
//!   in 314 s) rounded up.
//! - **× 1.15**: padding for hiccups and retries.
//!
//! This is THE one copy of the formula. The engine bounds every index it runs with it (a launch's
//! base re-index, the repo watcher's checkout re-index, onboarding's first index), and crew's
//! project-graph refresh reads it through core-ts (`Core.indexBudget`), so no second copy exists.
//! A project graph's budget is the sum of its member repos' budgets.
//!
//! A tree whose files cannot be listed by git (not a repository, git missing or hung) is counted
//! by a walk that skips `.git`: an unknown size never shrinks the budget to the floor.

use std::path::Path;
use std::time::Duration;

use wicked_apps_core::HardenedCommand;

/// The least any index gets, before padding.
pub const INDEX_BUDGET_FLOOR: Duration = Duration::from_secs(120);
/// The per-file allowance, before padding.
pub const INDEX_BUDGET_PER_FILE: Duration = Duration::from_millis(300);
/// The padding, in percent of the sized budget (115 = × 1.15).
pub const INDEX_BUDGET_PAD_PERCENT: u64 = 115;

/// How long `git ls-files` may take before the tree is counted by a walk instead.
const COUNT_GIT_BOUND: Duration = Duration::from_secs(30);

/// How the files were counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexBudgetCount {
    /// `git ls-files --cached --others --exclude-standard`.
    Git,
    /// A directory walk that skips `.git` (git could not list the tree).
    Walk,
    /// Not counted: a fixed budget (tests only).
    Fixed,
}

impl IndexBudgetCount {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexBudgetCount::Git => "git",
            IndexBudgetCount::Walk => "walk",
            IndexBudgetCount::Fixed => "fixed",
        }
    }
}

/// One tree's index budget and what it was sized from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexBudget {
    /// The files the budget was sized from (`None` only for [`IndexBudgetCount::Fixed`]).
    pub files: Option<u64>,
    pub counted: IndexBudgetCount,
    pub budget: Duration,
}

impl IndexBudget {
    /// A budget that is not sized from a tree (tests drive the bound directly).
    pub fn fixed(budget: Duration) -> IndexBudget {
        IndexBudget {
            files: None,
            counted: IndexBudgetCount::Fixed,
            budget,
        }
    }

    /// The budget in milliseconds (what the wire carries).
    pub fn budget_ms(&self) -> u64 {
        u64::try_from(self.budget.as_millis()).unwrap_or(u64::MAX)
    }

    /// `760s for 2203 files`: which budget an index hit, in the words a timeout reports.
    pub fn describe(&self) -> String {
        let secs = self.budget.as_secs();
        match self.files {
            Some(n) => format!("{secs}s for {n} files"),
            None => format!("{secs}s"),
        }
    }
}

/// THE formula: `max(120 s, files × 0.30 s) × 1.15`, in whole milliseconds (integer arithmetic, so
/// no float rounding and no overflow: every step saturates).
pub fn index_budget_for_files(files: u64) -> Duration {
    let floor_ms = INDEX_BUDGET_FLOOR.as_millis() as u64;
    let per_file_ms = INDEX_BUDGET_PER_FILE.as_millis() as u64;
    let sized_ms = floor_ms.max(files.saturating_mul(per_file_ms));
    Duration::from_millis(sized_ms.saturating_mul(INDEX_BUDGET_PAD_PERCENT) / 100)
}

/// The budget for indexing `tree`, sized from its file count (see the module doc).
pub fn index_budget(tree: &Path) -> IndexBudget {
    let (files, counted) = match git_file_count(tree) {
        Some(n) => (n, IndexBudgetCount::Git),
        None => (walk_file_count(tree), IndexBudgetCount::Walk),
    };
    IndexBudget {
        files: Some(files),
        counted,
        budget: index_budget_for_files(files),
    }
}

/// `git ls-files -z --cached --others --exclude-standard` in `tree`, counted; `None` when git cannot
/// list it within [`COUNT_GIT_BOUND`] (not a work tree, git missing, a failure).
fn git_file_count(tree: &Path) -> Option<u64> {
    if !tree.is_dir() {
        return None;
    }
    let mut child = std::process::Command::new("git")
        .hardened()
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(tree)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Drained on its own thread: a large listing fills the pipe long before git exits.
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 64 * 1024];
        let mut n: u64 = 0;
        loop {
            match out.read(&mut buf) {
                Ok(0) => return Some(n),
                Ok(k) => n += buf[..k].iter().filter(|&&b| b == 0).count() as u64,
                Err(_) => return None,
            }
        }
    });
    let deadline = std::time::Instant::now() + COUNT_GIT_BOUND;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let n = reader.join().ok()??;
    status.success().then_some(n)
}

/// Files under `tree`, skipping `.git` and never following a symlinked directory. Never capped (a
/// capped count would undersize the budget). A directory or entry it cannot read is not counted: the
/// indexer runs as the same user and cannot read it either, so it is not part of what is indexed.
fn walk_file_count(tree: &Path) -> u64 {
    let mut n: u64 = 0;
    let mut stack = vec![tree.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if entry.file_name() == ".git" {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ruling's worked examples (crew#1481): the floor, studio, crew and a 17.5k-file repo.
    #[test]
    fn the_budget_is_the_ruling_formula_in_milliseconds() {
        // max(120 s, 0) × 1.15 = 138 s: the floor is padded too.
        assert_eq!(index_budget_for_files(0), Duration::from_millis(138_000));
        // 400 files × 0.3 = 120 s: still the floor.
        assert_eq!(index_budget_for_files(400), Duration::from_millis(138_000));
        // One file past the floor sizes the budget from the files.
        assert_eq!(index_budget_for_files(401), Duration::from_millis(138_345));
        // wicked-studio, 2,203 files: 660.9 s × 1.15 = 760.035 s.
        assert_eq!(
            index_budget_for_files(2_203),
            Duration::from_millis(760_035)
        );
        // wicked-crew, 1,291 files: 387.3 s × 1.15 = 445.395 s.
        assert_eq!(
            index_budget_for_files(1_291),
            Duration::from_millis(445_395)
        );
        // 17,500 files: 5,250 s × 1.15 = 6,037.5 s (1 h 40 m).
        assert_eq!(
            index_budget_for_files(17_500),
            Duration::from_millis(6_037_500)
        );
        // Saturates, never wraps to a tiny budget.
        assert!(index_budget_for_files(u64::MAX) >= index_budget_for_files(17_500));
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wc-index-budget-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .hardened()
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A work tree is counted by git: tracked and untracked files count, ignored ones do not.
    #[test]
    fn a_work_tree_is_counted_by_git_ls_files() {
        let dir = scratch("git");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn b() {}").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "x"]);
        // Untracked, not ignored: read by a checkout index, so counted.
        std::fs::write(dir.join("c.rs"), "fn c() {}").unwrap();
        // Ignored: not counted.
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join("build/out.o"), "x").unwrap();
        let b = index_budget(&dir);
        assert_eq!(b.counted, IndexBudgetCount::Git);
        assert_eq!(b.files, Some(4), "{b:?}"); // .gitignore, a.rs, src/b.rs, c.rs
        assert_eq!(b.budget, index_budget_for_files(4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tree git cannot list is walked (skipping `.git`), never left at the floor unsized.
    #[test]
    fn a_tree_git_cannot_list_is_counted_by_a_walk() {
        let dir = scratch("walk");
        // A `.git` that is not a repository: git refuses it, the walk skips it.
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        std::fs::write(dir.join(".git/objects/x"), "x").unwrap();
        std::fs::create_dir_all(dir.join("deep/er")).unwrap();
        for i in 0..3 {
            std::fs::write(dir.join(format!("deep/er/f{i}")), "x").unwrap();
        }
        std::fs::write(dir.join("top"), "x").unwrap();
        let b = index_budget(&dir);
        assert_eq!(b.counted, IndexBudgetCount::Walk, "{b:?}");
        assert_eq!(b.files, Some(4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_budget_says_what_it_was_sized_from() {
        let b = IndexBudget {
            files: Some(2_203),
            counted: IndexBudgetCount::Git,
            budget: index_budget_for_files(2_203),
        };
        assert_eq!(b.describe(), "760s for 2203 files");
        assert_eq!(b.budget_ms(), 760_035);
        assert_eq!(IndexBudget::fixed(Duration::from_secs(1)).describe(), "1s");
    }
}
