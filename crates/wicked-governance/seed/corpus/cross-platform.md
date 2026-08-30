---
id: cross-platform
title: "Cross-platform shell + tooling rules"
status: active
date: 2026-08-29
enforcement_class: guidance
scope: wiki:architecture
domain: cross-platform
applies_to: [build, review]
---
# Cross-platform rules

Derived from the global Claude Code rules doc (`~/.claude/CLAUDE.md`, "Cross-Platform
Requirement"): all skills, hooks, agents, and shell commands written for the wicked workspace
must work on macOS/Linux AND Windows. Seed-corpus companion to arch-R9's cross-platform
doctrine domain.

## Rules

- `PAT-1901` (error): Never use printf or echo with backslash-escape interpretation for JSON
  or multi-line output — zsh/bash interpret escapes differently across platforms; use
  printf '%s' or Python.
- `PAT-1902` (error): Hook commands that output JSON go through python3 -c with a python -c
  fallback (covers macOS, Linux, WSL, and Windows Git Bash), never hand-quoted shell JSON.
- `PAT-1903` (warn): No Unix-only shell features in hook commands without a Windows fallback
  path; prefer Python over shell builtins for JSON, string manipulation, and file paths.
- `PAT-1904` (warn): Paths that become identifiers (provenance refs, graph locations) compare
  with forward slashes on every platform — normalize before comparing, never compare raw
  OS-native paths.

## Sources

- `~/.claude/CLAUDE.md` — "Cross-Platform Requirement" (global rules, applies workspace-wide).
- `wicked-core/crates/wicked-governance/src/markdown.rs` + `provenance.rs` — the forward-slash
  ref mandate as implemented.
