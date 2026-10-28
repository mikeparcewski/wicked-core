//! (core#1161) The lift's changelog resolution: a conflict in a changelog file is resolved the
//! way a person resolves it — the run's new `## [Unreleased]` entries are placed under main's
//! CURRENT `## [Unreleased]` heading, each under the same `### …` subsection it was written
//! under, and everything main did to the file (a release cut that renamed `[Unreleased]` to
//! `[X.Y.Z]`, new link-refs, its own new entries) is kept as main has it.
//!
//! Before this, the lift union-merged a changelog only when both sides were identical outside
//! `[Unreleased]`, so any release cut between a run's base and its delivery stranded every
//! in-flight run that added a changelog line (rig run 115fabdb).
//!
//! Fail-closed: the resolution is refused (and the conflict stands, recoverable) when the run
//! changed the file outside its `[Unreleased]` section in a way main did not, when the run
//! removed or rewrote an `[Unreleased]` line instead of only adding lines, or when main's file
//! has no `## [Unreleased]` heading to place the entries under.

/// A changelog file, by name: `CHANGELOG.md` or `CHANGES.md` in any case, at any depth.
pub(super) fn is_changelog(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.eq_ignore_ascii_case("CHANGELOG.md") || name.eq_ignore_ascii_case("CHANGES.md")
}

/// Whether `line` is the `## [Unreleased]` heading (the bracketed name in any case).
fn is_unreleased_heading(line: &[u8]) -> bool {
    const H: &[u8] = b"## [unreleased]";
    line.len() >= H.len() && line[..H.len()].eq_ignore_ascii_case(H)
}

/// A level-2 heading (`## …`): it ends the `[Unreleased]` section.
fn is_section_heading(line: &[u8]) -> bool {
    line.starts_with(b"## ")
}

/// A subsection heading inside `[Unreleased]` (`### Fixed`), trimmed, for matching.
fn subsection(line: &[u8]) -> Option<String> {
    line.starts_with(b"### ")
        .then(|| String::from_utf8_lossy(line).trim().to_string())
}

fn is_blank(line: &[u8]) -> bool {
    line.iter().all(|b| b.is_ascii_whitespace())
}

/// A changelog split around its first `## [Unreleased]` section: the lines before the heading,
/// the heading line, the section body (up to, not including, the next `## ` heading), and the
/// rest. `heading` is `None` when the file has no `[Unreleased]` section (then `before` holds
/// every line and `body`/`after` are empty).
struct Split<'a> {
    before: Vec<&'a [u8]>,
    heading: Option<&'a [u8]>,
    body: Vec<&'a [u8]>,
    after: Vec<&'a [u8]>,
}

impl<'a> Split<'a> {
    fn of(text: &'a [u8]) -> Self {
        let mut s = Split {
            before: Vec::new(),
            heading: None,
            body: Vec::new(),
            after: Vec::new(),
        };
        let mut lines = text.split_inclusive(|b| *b == b'\n');
        for line in lines.by_ref() {
            if is_unreleased_heading(line) {
                s.heading = Some(line);
                break;
            }
            s.before.push(line);
        }
        let mut lines = lines.peekable();
        while let Some(line) = lines.next_if(|l| !is_section_heading(l)) {
            s.body.push(line);
        }
        s.after.extend(lines);
        s
    }

    /// Everything outside the `[Unreleased]` section (the heading line excluded).
    fn outside(&self) -> Vec<&'a [u8]> {
        self.before.iter().chain(&self.after).copied().collect()
    }
}

/// The longest common subsequence of `a` and `b`, as `kept[i]` = whether `b[i]` is matched
/// (not inserted), and whether every non-blank line of `a` was matched (nothing removed).
/// `None` when the sections are too large to compare in memory.
fn insertions(a: &[&[u8]], b: &[&[u8]]) -> Option<(Vec<bool>, bool)> {
    const MAX_CELLS: usize = 16_000_000;
    let (n, m) = (a.len(), b.len());
    if (n + 1).saturating_mul(m + 1) > MAX_CELLS {
        return None;
    }
    let eq = |x: &[u8], y: &[u8]| x.trim_ascii_end() == y.trim_ascii_end();
    // lcs[i][j] = LCS length of a[i..] and b[j..].
    let w = m + 1;
    let mut lcs = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * w + j] = if eq(a[i], b[j]) {
                lcs[(i + 1) * w + j + 1] + 1
            } else {
                lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut matched_b = vec![false; m];
    let mut removed_content = false;
    while i < n && j < m {
        if eq(a[i], b[j]) {
            matched_b[j] = true;
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * w + j] >= lcs[i * w + j + 1] {
            removed_content |= !is_blank(a[i]);
            i += 1;
        } else {
            j += 1;
        }
    }
    removed_content |= a[i..].iter().any(|l| !is_blank(l));
    Some((matched_b, !removed_content))
}

/// The run's new entries, grouped by the `### …` subsection they sit under in the run's
/// `[Unreleased]` (`None` = directly under the `## [Unreleased]` heading), in the run's order.
struct Group {
    heading: Option<String>,
    /// The run wrote a blank line between this subsection's heading and its first entry.
    blank_after_heading: bool,
    lines: Vec<Vec<u8>>,
}

fn groups(body: &[&[u8]], kept: &[bool]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut current: Option<String> = None;
    let mut blank_after: bool = false;
    for (i, line) in body.iter().enumerate() {
        if let Some(h) = subsection(line) {
            current = Some(h);
            blank_after = body.get(i + 1).is_some_and(|l| is_blank(l));
            continue;
        }
        if kept[i] {
            continue;
        }
        let mut owned = line.to_vec();
        if !owned.ends_with(b"\n") {
            owned.push(b'\n');
        }
        match out.iter_mut().find(|g| g.heading == current) {
            Some(g) => g.lines.push(owned),
            None => out.push(Group {
                heading: current.clone(),
                blank_after_heading: blank_after,
                lines: vec![owned],
            }),
        }
    }
    for g in &mut out {
        // Blank lines only separate entries: none leads or trails a group.
        while g.lines.first().is_some_and(|l| is_blank(l)) {
            g.lines.remove(0);
        }
        while g.lines.last().is_some_and(|l| is_blank(l)) {
            g.lines.pop();
        }
    }
    out.retain(|g| !g.lines.is_empty());
    out
}

/// The index just past the last non-blank line of `body[from..to]` (`from` when all blank).
fn content_end(body: &[Vec<u8>], from: usize, to: usize) -> usize {
    (from..to)
        .rev()
        .find(|&i| !is_blank(&body[i]))
        .map_or(from, |i| i + 1)
}

/// Entries into an `[Unreleased]` body with no entries of its own: one blank line under the
/// heading, the entries, and one blank line before whatever heading follows.
fn into_empty(body: &mut Vec<Vec<u8>>, mut block: Vec<Vec<u8>>) {
    let had_blank = body.first().is_some_and(|l| is_blank(l));
    if !had_blank {
        block.insert(0, b"\n".to_vec());
    }
    let start = usize::from(had_blank);
    let trailing = body.len() > start;
    body.splice(start..start, block);
    if !trailing {
        body.push(b"\n".to_vec());
    }
}

/// Place one group of the run's entries into main's `[Unreleased]` body.
fn place(body: &mut Vec<Vec<u8>>, group: &Group) {
    // An entry main already has in its `[Unreleased]` (both sides added the same line) is kept once.
    let present = |l: &[u8]| {
        body.iter()
            .any(|b| !is_blank(b) && b.trim_ascii_end() == l.trim_ascii_end())
    };
    let lines: Vec<Vec<u8>> = group
        .lines
        .iter()
        .filter(|l| is_blank(l) || !present(l))
        .cloned()
        .collect();
    if lines.iter().all(|l| is_blank(l)) {
        return;
    }
    let first_sub = body.iter().position(|l| subsection(l).is_some());
    match &group.heading {
        // Directly under `## [Unreleased]`: after main's own lines there, before its first `###`.
        None => {
            let to = first_sub.unwrap_or(body.len());
            let at = content_end(body, 0, to);
            if at == 0 && first_sub.is_none() {
                into_empty(body, lines);
                return;
            }
            let mut block = lines;
            if first_sub == Some(at) {
                // Entries placed right above main's first subsection: a blank line between.
                block.push(b"\n".to_vec());
            }
            if at == 0 && body.first().is_some_and(|l| is_blank(l)) {
                // Keep the blank line under the heading above the entries.
                body.splice(1..1, block);
                return;
            }
            body.splice(at..at, block);
        }
        Some(h) => {
            let found = body
                .iter()
                .position(|l| subsection(l).as_deref() == Some(h.as_str()));
            match found {
                // Main has the subsection: at the end of its entries.
                Some(i) => {
                    let next = (i + 1..body.len())
                        .find(|&k| subsection(&body[k]).is_some())
                        .unwrap_or(body.len());
                    let at = content_end(body, i + 1, next);
                    body.splice(at..at, lines);
                }
                // A subsection main lacks: a new one after main's last entry.
                None => {
                    let at = content_end(body, 0, body.len());
                    let mut block = Vec::new();
                    if at > 0 {
                        block.push(b"\n".to_vec());
                    }
                    block.push(format!("{h}\n").into_bytes());
                    if group.blank_after_heading {
                        block.push(b"\n".to_vec());
                    }
                    block.extend(lines);
                    if at == 0 {
                        into_empty(body, block);
                        return;
                    }
                    body.splice(at..at, block);
                }
            }
        }
    }
}

/// Resolve one changelog the way a person would: main's file (`ours`), with the run's (`theirs`)
/// new `[Unreleased]` entries since `base` placed under main's `## [Unreleased]`. `Err(why)` when
/// it cannot be done safely (see the module docs); the conflict then stands.
pub(super) fn resolve(base: &[u8], ours: &[u8], theirs: &[u8]) -> Result<Vec<u8>, String> {
    let (b, o, t) = (Split::of(base), Split::of(ours), Split::of(theirs));
    if t.heading.is_none() {
        return Err("the run's copy has no `## [Unreleased]` section".to_string());
    }
    let t_out = t.outside();
    if t_out != b.outside() && t_out != o.outside() {
        return Err("the run changed it outside its `## [Unreleased]` section".to_string());
    }
    let Some(o_heading) = o.heading else {
        return Err(
            "main's copy has no `## [Unreleased]` section to place the run's entries under"
                .to_string(),
        );
    };
    let (kept, additions_only) = insertions(&b.body, &t.body)
        .ok_or_else(|| "its `[Unreleased]` section is too large to compare".to_string())?;
    if !additions_only {
        return Err(
            "the run removed or rewrote a line of its `[Unreleased]` section (only added lines \
             are placed)"
                .to_string(),
        );
    }
    let mut body: Vec<Vec<u8>> = o.body.iter().map(|l| l.to_vec()).collect();
    // A body that ends the file without a newline gets one before entries are placed after it.
    if let Some(last) = body.last_mut() {
        if !last.ends_with(b"\n") {
            last.push(b'\n');
        }
    }
    for g in groups(&t.body, &kept) {
        place(&mut body, &g);
    }
    let mut out = Vec::with_capacity(ours.len() + theirs.len());
    for l in &o.before {
        out.extend_from_slice(l);
    }
    out.extend_from_slice(o_heading);
    if !o_heading.ends_with(b"\n") {
        out.push(b'\n');
    }
    for l in &body {
        out.extend_from_slice(l);
    }
    for l in &o.after {
        out.extend_from_slice(l);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(base: &str, ours: &str, theirs: &str) -> Result<String, String> {
        resolve(base.as_bytes(), ours.as_bytes(), theirs.as_bytes())
            .map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn changelog_files_are_named_changelog_or_changes_in_any_case_at_any_depth() {
        for p in [
            "CHANGELOG.md",
            "changelog.md",
            "packages/x/Changelog.MD",
            "CHANGES.md",
            "a/b/changes.md",
        ] {
            assert!(is_changelog(p), "{p}");
        }
        for p in [
            "CHANGELOG.md.bak",
            "docs/CHANGELOG.txt",
            "MY-CHANGELOG.md",
            "CHANGES",
        ] {
            assert!(!is_changelog(p), "{p}");
        }
    }

    /// The 115fabdb shape: main cut a release (`[Unreleased]` → `[0.16.7]` + link-refs) while the
    /// run added a `### Fixed` bullet. The bullet lands under main's new, empty `[Unreleased]`.
    #[test]
    fn a_release_cut_keeps_main_and_moves_the_runs_entry_under_the_new_unreleased() {
        let base = "# C\n\n## [Unreleased]\n\n### Fixed\n\n- old fix\n\n## [0.16.6]\n\n- six\n\n[Unreleased]: u...0.16.6\n";
        let ours = "# C\n\n## [Unreleased]\n\n## [0.16.7] - 2026-10-28\n\n### Fixed\n\n- old fix\n\n## [0.16.6]\n\n- six\n\n[Unreleased]: u...0.16.7\n[0.16.7]: u/0.16.6...0.16.7\n";
        let theirs = "# C\n\n## [Unreleased]\n\n### Fixed\n\n- old fix\n- run fix\n\n## [0.16.6]\n\n- six\n\n[Unreleased]: u...0.16.6\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "# C\n\n## [Unreleased]\n\n### Fixed\n\n- run fix\n\n## [0.16.7] - 2026-10-28\n\n### Fixed\n\n- old fix\n\n## [0.16.6]\n\n- six\n\n[Unreleased]: u...0.16.7\n[0.16.7]: u/0.16.6...0.16.7\n"
        );
    }

    #[test]
    fn both_sides_adding_to_unreleased_keep_both_entries_in_their_subsections() {
        let base = "# C\n\n## [Unreleased]\n\n### Fixed\n\n- a\n\n## [1.0.0]\n\n- first\n";
        let ours =
            "# C\n\n## [Unreleased]\n\n### Fixed\n\n- a\n- main fix\n\n## [1.0.0]\n\n- first\n";
        let theirs = "# C\n\n## [Unreleased]\n\n### Added\n\n- run feature\n\n### Fixed\n\n- a\n- run fix\n\n## [1.0.0]\n\n- first\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "# C\n\n## [Unreleased]\n\n### Fixed\n\n- a\n- main fix\n- run fix\n\n### Added\n\n- run feature\n\n## [1.0.0]\n\n- first\n"
        );
    }

    #[test]
    fn entries_directly_under_unreleased_and_a_shared_line_are_placed_once() {
        let base = "## [Unreleased]\n\n## [1.0.0]\n- first\n";
        let ours = "## [Unreleased]\n\n- main\n- same\n\n## [1.0.0]\n- first\n";
        let theirs = "## [Unreleased]\n\n- same\n- run\n\n## [1.0.0]\n- first\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "## [Unreleased]\n\n- main\n- same\n- run\n\n## [1.0.0]\n- first\n"
        );
        // Into an empty section, with no blank line in main's copy.
        let ours = "## [Unreleased]\n## [1.0.1]\n- landed\n## [1.0.0]\n- first\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "## [Unreleased]\n\n- same\n- run\n\n## [1.0.1]\n- landed\n## [1.0.0]\n- first\n"
        );
    }

    #[test]
    fn a_multi_line_entry_keeps_its_continuation_lines() {
        let base = "## [Unreleased]\n\n## [1.0.0]\n";
        let ours = "## [Unreleased]\n\n## [1.1.0]\n\n## [1.0.0]\n";
        let theirs =
            "## [Unreleased]\n\n### Fixed\n- run: a long entry\n  that wraps\n\n## [1.0.0]\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "## [Unreleased]\n\n### Fixed\n- run: a long entry\n  that wraps\n\n## [1.1.0]\n\n## [1.0.0]\n"
        );
    }

    #[test]
    fn refusals_name_why() {
        let base = "## [Unreleased]\n\n- a\n\n## [1.0.0]\n- first\n";
        let ours = "## [Unreleased]\n\n- a\n- main\n\n## [1.0.0]\n- first\n";
        // The run edited a released entry main did not.
        let e = r(
            base,
            ours,
            "## [Unreleased]\n\n- a\n- run\n\n## [1.0.0]\n- first!\n",
        )
        .unwrap_err();
        assert!(e.contains("outside"), "{e}");
        // The run rewrote an `[Unreleased]` line instead of adding one.
        let e = r(
            base,
            ours,
            "## [Unreleased]\n\n- a (reworded)\n\n## [1.0.0]\n- first\n",
        )
        .unwrap_err();
        assert!(e.contains("removed or rewrote"), "{e}");
        // Main dropped the `[Unreleased]` heading altogether.
        let e = r(
            base,
            "## [1.1.0]\n\n- a\n\n## [1.0.0]\n- first\n",
            "## [Unreleased]\n\n- a\n- run\n\n## [1.0.0]\n- first\n",
        )
        .unwrap_err();
        assert!(e.contains("no `## [Unreleased]`"), "{e}");
    }

    #[test]
    fn the_run_and_main_making_the_same_outside_change_is_not_a_refusal() {
        let base = "## [Unreleased]\n\n## [1.0.0]\n- first\n";
        let both = "## [Unreleased]\n\n- main\n\n## [1.0.0]\n- first (fixed)\n";
        let theirs = "## [Unreleased]\n\n- run\n\n## [1.0.0]\n- first (fixed)\n";
        assert_eq!(
            r(base, both, theirs).unwrap(),
            "## [Unreleased]\n\n- main\n- run\n\n## [1.0.0]\n- first (fixed)\n"
        );
    }
}
