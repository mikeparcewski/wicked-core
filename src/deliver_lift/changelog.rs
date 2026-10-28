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
//! The sections are compared ENTRY by entry ([`Entry`]: a bullet with its indented
//! continuation), never line by line, so an added entry is placed whole.
//!
//! Fail-closed: the resolution is refused (and the conflict stands, recoverable) when the run
//! changed the file outside its `[Unreleased]` section in a way main did not, when the run
//! removed or rewrote an `[Unreleased]` entry instead of only adding entries, or when main's file
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

/// One changelog entry: a line that starts at the margin (a bullet, or a line of prose) with
/// its continuation — every following indented line, and the blank lines between them (a
/// multi-paragraph entry). Entries, not lines, are what the resolution compares and places, so an
/// entry is never split and a continuation shared by two entries never merges them.
struct Entry {
    /// The `### …` subsection it sits under (`None` = directly under `## [Unreleased]`).
    heading: Option<String>,
    lines: Vec<Vec<u8>>,
    /// Its text for comparison: the non-blank lines, trailing whitespace trimmed.
    key: Vec<u8>,
    /// A blank line separates it from the entry before it.
    spaced: bool,
}

fn is_indented(line: &[u8]) -> bool {
    matches!(line.first(), Some(b' ' | b'\t'))
}

fn owned(line: &[u8]) -> Vec<u8> {
    let mut l = line.to_vec();
    if !l.ends_with(b"\n") {
        l.push(b'\n');
    }
    l
}

/// The entries of an `[Unreleased]` body, in order.
fn entries(body: &[&[u8]]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut heading: Option<String> = None;
    let (mut blank_before, mut after_entry) = (false, false);
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        if let Some(h) = subsection(line) {
            heading = Some(h);
            (blank_before, after_entry) = (false, false);
            i += 1;
            continue;
        }
        if is_blank(line) {
            blank_before = true;
            i += 1;
            continue;
        }
        let mut lines = vec![owned(line)];
        i += 1;
        loop {
            let mut j = i;
            while j < body.len() && is_blank(body[j]) {
                j += 1;
            }
            if j < body.len() && is_indented(body[j]) {
                lines.extend(body[i..=j].iter().map(|l| owned(l)));
                i = j + 1;
            } else {
                break;
            }
        }
        let key = lines
            .iter()
            .filter(|l| !is_blank(l))
            .flat_map(|l| l.trim_ascii_end().iter().copied().chain(*b"\n"))
            .collect();
        out.push(Entry {
            heading: heading.clone(),
            lines,
            key,
            spaced: blank_before && after_entry,
        });
        (blank_before, after_entry) = (false, true);
    }
    out
}

/// The run's new entries under one subsection, in the run's order.
struct Group<'e> {
    heading: Option<String>,
    /// The run wrote a blank line between this subsection's heading and its first entry.
    blank_after_heading: bool,
    entries: Vec<&'e Entry>,
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

/// Place one group of the run's entries into main's `[Unreleased]` body. `spaced`: the file
/// separates entries with a blank line.
fn place(body: &mut Vec<Vec<u8>>, group: &Group, spaced: bool) {
    let sep = |block: &mut Vec<Vec<u8>>| {
        if spaced {
            block.push(b"\n".to_vec());
        }
    };
    let mut entries = Vec::new();
    for (k, e) in group.entries.iter().enumerate() {
        if k > 0 {
            sep(&mut entries);
        }
        entries.extend(e.lines.iter().cloned());
    }
    let first_sub = body.iter().position(|l| subsection(l).is_some());
    match &group.heading {
        // Directly under `## [Unreleased]`: after main's own entries there, before its first `###`.
        None => {
            let to = first_sub.unwrap_or(body.len());
            let at = content_end(body, 0, to);
            if at > 0 {
                let mut block = Vec::new();
                sep(&mut block);
                block.extend(entries);
                body.splice(at..at, block);
            } else if first_sub.is_none() {
                into_empty(body, entries);
            } else {
                // Nothing directly under the heading yet, then main's first subsection: one blank
                // line under the heading, the entries, one blank line before the subsection.
                let ins = usize::from(body.first().is_some_and(|l| is_blank(l)));
                let mut block = Vec::new();
                if ins == 0 {
                    block.push(b"\n".to_vec());
                }
                block.extend(entries);
                block.push(b"\n".to_vec());
                body.splice(ins..ins, block);
            }
        }
        Some(h) => {
            let found = body
                .iter()
                .position(|l| subsection(l).as_deref() == Some(h.as_str()));
            match found {
                // Main has the subsection: after its last entry.
                Some(i) => {
                    let next = (i + 1..body.len())
                        .find(|&k| subsection(&body[k]).is_some())
                        .unwrap_or(body.len());
                    let at = content_end(body, i + 1, next);
                    let mut block = Vec::new();
                    if at > i + 1 {
                        sep(&mut block);
                    } else if group.blank_after_heading {
                        block.push(b"\n".to_vec());
                    }
                    block.extend(entries);
                    if at == next && next < body.len() {
                        // An empty subsection right above the next one: keep them apart.
                        block.push(b"\n".to_vec());
                    }
                    body.splice(at..at, block);
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
                    block.extend(entries);
                    if at == 0 {
                        into_empty(body, block);
                    } else {
                        body.splice(at..at, block);
                    }
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
    // The run's additions: its entries less the base's (as a multiset, by text). A base entry the
    // run no longer has was removed or rewritten — refused.
    let (b_entries, o_entries, t_entries) = (entries(&b.body), entries(&o.body), entries(&t.body));
    let mut unmatched: Vec<&Entry> = b_entries.iter().collect();
    let mut added: Vec<&Entry> = Vec::new();
    for e in &t_entries {
        match unmatched.iter().position(|u| u.key == e.key) {
            Some(k) => {
                unmatched.remove(k);
            }
            None => added.push(e),
        }
    }
    if !unmatched.is_empty() {
        return Err(
            "the run removed or rewrote an entry of its `[Unreleased]` section (only added \
             entries are placed)"
                .to_string(),
        );
    }
    // An entry main already has under the same subsection (both sides added it) is kept once.
    added.retain(|e| {
        !o_entries
            .iter()
            .any(|m| m.heading == e.heading && m.key == e.key)
    });
    let mut groups: Vec<Group> = Vec::new();
    for e in added {
        match groups.iter_mut().find(|g| g.heading == e.heading) {
            Some(g) => g.entries.push(e),
            None => {
                let blank_after_heading = e.heading.as_ref().is_some_and(|h| {
                    t.body
                        .iter()
                        .position(|l| subsection(l).as_deref() == Some(h.as_str()))
                        .and_then(|i| t.body.get(i + 1))
                        .is_some_and(|l| is_blank(l))
                });
                groups.push(Group {
                    heading: e.heading.clone(),
                    blank_after_heading,
                    entries: vec![e],
                })
            }
        }
    }
    let spaced = [&b_entries, &o_entries, &t_entries]
        .iter()
        .any(|es| es.iter().any(|e| e.spaced));
    let mut body: Vec<Vec<u8>> = o.body.iter().map(|l| l.to_vec()).collect();
    // A body that ends the file without a newline gets one before entries are placed after it.
    if let Some(last) = body.last_mut() {
        if !last.ends_with(b"\n") {
            last.push(b'\n');
        }
    }
    for g in &groups {
        place(&mut body, g, spaced);
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
        assert!(e.contains("removed or rewrote an entry"), "{e}");
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

    /// (xreview codex P2 #1) Two distinct entries that share a continuation line are both kept
    /// whole; an identical entry in ANOTHER subsection is not taken for a duplicate.
    #[test]
    fn a_shared_continuation_or_a_same_text_entry_elsewhere_is_not_dropped() {
        let base = "## [Unreleased]\n\n## [1.0.0]\n";
        let ours = "## [Unreleased]\n\n### Fixed\n- main: corrected X\n  for all users.\n- same\n\n## [1.0.0]\n";
        let theirs = "## [Unreleased]\n\n### Fixed\n- run: corrected Y\n  for all users.\n\n### Added\n- same\n\n## [1.0.0]\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "## [Unreleased]\n\n### Fixed\n- main: corrected X\n  for all users.\n- same\n- run: corrected Y\n  for all users.\n\n### Added\n- same\n\n## [1.0.0]\n"
        );
    }

    /// (xreview codex P2 #2) A new entry whose first line repeats an existing one is placed whole,
    /// never as fragments of the two.
    #[test]
    fn a_new_entry_repeating_an_existing_bullet_is_placed_whole() {
        let base =
            "## [Unreleased]\n\n### Fixed\n- Fix startup\n  on Linux.\n\n## [1.0.0]\n- one\n";
        let ours = "## [Unreleased]\n\n## [1.1.0]\n\n### Fixed\n- Fix startup correctly\n  on Linux.\n\n## [1.0.0]\n- one\n";
        let theirs = "## [Unreleased]\n\n### Fixed\n- Fix startup\n  on Windows.\n\n- Fix startup\n  on Linux.\n\n## [1.0.0]\n- one\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "## [Unreleased]\n\n### Fixed\n- Fix startup\n  on Windows.\n\n## [1.1.0]\n\n### Fixed\n- Fix startup correctly\n  on Linux.\n\n## [1.0.0]\n- one\n"
        );
    }

    /// (xreview agy P2) Blank lines are not entries: a run that only moved a blank line while adding
    /// an entry is not refused, and a multi-paragraph entry keeps its inner blank line.
    #[test]
    fn blank_lines_neither_refuse_an_addition_nor_split_a_multi_paragraph_entry() {
        let base = "## [Unreleased]\n\n### Fixed\n- old fix\n\n## [1.0.0]\n";
        let ours = "## [Unreleased]\n\n## [1.1.0]\n\n### Fixed\n- old fix\n\n## [1.0.0]\n";
        let theirs = "## [Unreleased]\n\n### Fixed\n\n- old fix\n- run fix\n  first paragraph\n\n  second paragraph\n## [1.0.0]\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "## [Unreleased]\n\n### Fixed\n\n- run fix\n  first paragraph\n\n  second paragraph\n\n## [1.1.0]\n\n### Fixed\n- old fix\n\n## [1.0.0]\n"
        );
    }

    /// (xreview agy P3) An entry placed directly under the heading, above main's first subsection,
    /// keeps a blank line before that subsection.
    #[test]
    fn an_entry_above_mains_first_subsection_keeps_a_blank_line_before_it() {
        let base = "## [Unreleased]\n\n## [1.0.0]\n";
        let ours = "## [Unreleased]\n\n### Fixed\n\n- main fix\n\n## [1.0.0]\n";
        let theirs = "## [Unreleased]\n\n- run entry\n\n## [1.0.0]\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "## [Unreleased]\n\n- run entry\n\n### Fixed\n\n- main fix\n\n## [1.0.0]\n"
        );
    }

    /// A changelog that separates its entries with blank lines (this repository's own) gets the
    /// run's entry separated the same way.
    #[test]
    fn blank_separated_entries_stay_blank_separated() {
        let base = "## [Unreleased]\n\n- **a**\n  more a\n\n- **b**\n\n## [1.0.0]\n";
        let ours = "## [Unreleased]\n\n- **main**\n\n- **a**\n  more a\n\n- **b**\n\n## [1.0.0]\n";
        let theirs = "## [Unreleased]\n\n- **run**\n  more run\n\n- **a**\n  more a\n\n- **b**\n\n## [1.0.0]\n";
        assert_eq!(
            r(base, ours, theirs).unwrap(),
            "## [Unreleased]\n\n- **main**\n\n- **a**\n  more a\n\n- **b**\n\n- **run**\n  more run\n\n## [1.0.0]\n"
        );
    }
}
