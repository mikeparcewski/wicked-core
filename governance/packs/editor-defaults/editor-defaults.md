---
id: editor-defaults
title: Artifact editor permission defaults
status: active
date: 2026-10-02
enforcement_class: policy
steering_type: security
scope: wiki:governance
domain: editors
confidence: 1.0
---

# Artifact editor permission defaults

An artifact editor is a plugin studio hosts in a sandboxed frame (DES-artifact-editor-plugins
§6). What it may do is decided by core (`editor_gate::evaluate_editor_grants`) on the same
steering engine the tool gate and the MCP gate use: `select_any` over the editor's subject tokens,
then `decide`, deny dominates. Crew answers `GET /api/v1/editors/:id/grants?project=` with the
decided set, and studio's host enforces it on every request. The plugin's own list is informative
only.

The subject tokens of one check are `editor`, `editor:<id>`, `editor:<id>/<permission>`,
`editor-perm:<permission>`, `project:<id>` when the check is for a project, and
`editor-origin:first-party` for a `wicked-*` editor whose entry hash is the one studio ships.
An operator writes ordinary steering rules over them. For example, "in Kestrel, no editor reads
checks" is a `deny` rule with `applies_to: [editor-perm:checks.read]` and a trigger on
`"project":"<kestrel id>"`. A deny dominates every approval.

## The answer per permission

| Outcome | When |
|---|---|
| `deny` | a rule denied it (deny dominates the ledger and every default) |
| `allow` | the approvals ledger holds the editor's exact token, or an `allow` rule fired (the defaults below) |
| `ask` | anything else: the host shows the permission at install and waits for the operator |

## Posture rules

The operator-editable posture ships as rules in `rules/editor-defaults.json`. The daemon seeds them
into the store at boot, insert-only, so a restart never undoes an approval or brings back a rule the
operator retired.

| Rule | Effect |
|---|---|
| `EDITOR-GRANTS` | The approvals ledger, not a decide-lane rule. Allowing a permission adds `editor:<id>@<version>#<sha256>/<permission>` to its `excludes`, using the full 64-hex sha256 of the entry file and the version. A new version or a changed hash is a different token and asks again. The engine reads it whether the rule is active or retired. |
| `EDITOR-BUILTIN` | Allows a first-party editor its default set (every permission but `network.media`). |
| `EDITOR-BUILTIN-PAGE-MEDIA` | Allows `network.media` for the first-party page editor only. |
| `EDITOR-OPEN-DEFAULTS` | Allows every editor `artifact.read`, `selection.chip`, `checks.contribute` and `ui.fullscreen`. |

## Rules

- `ED-1` (critical): An editor without `artifact.write` changes nothing. It can still point
  (chips) and note (advisory checks). Enforced by studio's host, from the grants this gate decides.
- `ED-2` (critical): With or without grants, an editor never sends, launches, approves, decides,
  delivers or remembers, and never reaches the network by fetch, XHR, WebSocket, EventSource,
  beacon, form or navigation. No message exists for these, and the editor's own policy carries
  `connect-src 'none'` and `form-action 'none'`.
- `ED-3` (critical): An editor never sees ids, file paths, URLs, run roots, tokens, other artifacts
  or other projects. Content is passed in, stripped of paths.
- `ED-4` (critical): An evaluator verdict is never authored by an editor. Its contributed checks
  are advisory and never counted.
