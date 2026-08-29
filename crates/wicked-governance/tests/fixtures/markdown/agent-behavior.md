---
id: agent-behavior
title: Agent behavior rules
scope: wiki:architecture
applies_to: [plan, build]
domain: agent-behavior
confidence: 0.9
targets:
  language: rust
---

# Agent behavior rules

Doctrine prose lives outside the Rules section and is ignored by the rule
parser (knowledge-side ingest of prose is the garden mem-ingest lane).

## Rules

- `PAT-001` (error): Never use `printf` without `%s` in cross-platform shell.
- `POL-002` (critical): All store writes go through the single-writer actor,
  never a competing direct connection.

## Appendix

More prose after the Rules section ends at the next heading.
