# Governance policy packs

A **pack** is one directory of governed doctrine in the `wicked-core rules ingest` layout —
frontmattered markdown rule docs (the AW-3 `MarkdownAdapter` convention, one parse path), plus
optional `rules/*.json` and `policies/*.json` lanes. Each pack is a self-contained ruleset a
consumer ingests into a store:

```
wicked-core rules ingest governance/packs/<pack> --db <store>
```

and recalls as a severity-ordered report (the AW-17 CI conformance seam v1):

```
wicked-core rules recall --db <store> [--json]
```

Every recalled rule cites its id (`PAT-`/`POL-`, INV-C1) and its provenance ref
(`<doc path>@<git blob sha>#<RULE-ID>`) — the wiki URI a CI comment links back to.

## Conventions

- **Git is the source of truth for a pack** (arch-R8): a pack changes only by doc PR — there is
  deliberately no `rules.write` MCP tool, and no agent-side promotion path. The graph copy is a
  rebuildable projection; `rules ingest` is idempotent and id-keyed, so re-ingest on merge is a
  non-event. (Steering's other authoring lane — governed UI/chat writes through wicked-crew's
  API — is first-class for daemon-held rules, but a PACK rule's home is its doc: provenance
  `path@sha#id` says so. See `crates/wicked-governance/STEERING.md`.)
- **One doc = one doctrine area**, frontmatter carries `id`, `title`, `status`,
  `enforcement_class` (policy | validator | guidance, arch-R4), `steering_type` (one of the
  seven steering types — architecture | development | security | testing | operations |
  compliance | design-ux; omitted defaults to `architecture`) and optional `applies_to`
  phase ids. Rule items live under a `## Rules` section: `` - `POL-NNNN` (severity): statement ``.
  Prose everywhere else is rationale — ignored by the rule parser, ingestable into the knowledge
  lane via garden mem-ingest.
- **Enforcement class is honest**: `policy` marks rules whose enforcement is deterministic —
  either a `wicked-governance` Policy trigger or a named engine gate. A pack rule NEVER claims an
  enforcement that no gate holds (the core#296 lesson: a prompt that says "(enforced)" and isn't
  is worse than silence). When the enforcing gate is engine-owned, the rule statement names the
  engine rule id (e.g. `engine:pre-build-scope`) so a denial's record and the doctrine doc
  cross-reference each other.

## Packs

| Pack | Steering type | Doctrine | Enforcing gate |
|---|---|---|---|
| [`phase-scope/`](phase-scope/phase-scope.md) | `operations` | Pre-build phases write documentation only (phase-scope write-denies) | `engine:pre-build-scope` (`gate_hook::phase_scope_denial`, core#306) + completion-path `phase_scope_warning` backstop |
