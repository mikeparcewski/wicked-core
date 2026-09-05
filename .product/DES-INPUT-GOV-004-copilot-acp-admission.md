# DES-INPUT-GOV-004 — Resolve OQ-COPILOT-ACP-001: copilot ACP input-governance admission

**Issue:** wicked-core #369
**Phase:** design (analysis + design + plan). **No production code in this deliverable.**
**Predecessors:**
- DES-INPUT-GOV-001 (recon, #360) — §OQ table defines OQ-COPILOT-ACP-001 and its proof properties
  (blocking request / canonical name+rawInput / reject honoured / disableable auto-approve; plus
  "separately record the native bounded-sandbox result needed by the wrapped-fallback issue" and
  "do not admit Copilot to `AcpGate` on lifecycle/streaming evidence alone").
- DES-INPUT-GOV-002 (#364) — replaced the Claude-name admission predicate with a per-seat,
  evidence-gated `acp_input_governance` capability that defaults **OFF**. This design resolves one
  seat's gate; it changes no admission mechanism.
- DES-INPUT-GOV-003 (codex, #367) / OQ-PI-ACP-001 (#368/#373) — the precedent this mirrors:
  evidence + verdict + a registry comment update, `acp_input_governance` left `false` with no test
  change.

**Carrier:** `src/acp_permission.rs` (FINDING-062, `pretool_payload` at `:56`) is CLI-agnostic and
unchanged by this work. The decision is about *who is admitted to that carrier*, not the carrier.

**Evidence (the SUBJECT of this design, produced by the clarify-phase capture unit):**
`.product/evidence/oq-copilot-acp-001/` — `manifest.md`, `verdict.md`, `README.md` (redaction), six
redacted `*.ndjson` captures, and the `*.mjs` harnesses. Raw captures stay in gitignored `tmp/`. All
committed evidence is REDACTED (no usernames/handles/emails/home paths).

**Operator amendment folded in (this phase):** approved the clarify phase's NOT ADMITTED verdict,
with two additions: (1) the verdict must record precisely that copilot's shape is materially
different from `pi`/`codex` — writes/edit/bash-class intents genuinely produce blocking client-side
requests, and the auto-approve control is off by default and behaves exactly as documented; (2) a
new **ADMISSION-CALCULUS** analysis (§4 below) of what the one real gap (ungated in-workspace reads)
actually costs, and an explicit filing — not a resolution — of the scoped-admission question it
raises.

---

## 1. Decision

**copilot is NOT ADMITTED. `acp_input_governance` stays `false` for the built-in `copilot` seat.**

The decision requires **no field change** — the flag is already `false` (DES-INPUT-GOV-002 set it so
for every non-claude seat). The only production change deferred to the implementation phase is the
**registry comment** on the `copilot` `AcpConfig`, updated to cite this verdict and lead with the
headline finding, mirroring the OQ-CODEX-ACP-001 → `registry.rs` update.

### Headline (why this is a near-pass, not a repeat of pi/codex)

Unlike `pi-acp` (no tool-serving permission path exists at all) and `codex-acp` (the path exists but
is routed around by the adapter's own internal reviewer for essentially every intent), **copilot's
native `--acp` mode — the exact invocation the registry already uses, no extra flags — genuinely
blocks on `session/request_permission` for every edit and every bash-class (execute) tool call
observed**, across an ordinary edit, a file create, an ordinary shell command, an explicitly
destructive `rm -rf`, and a network `curl` fetch (six independent tool calls, zero exceptions). A
selected reject was proven to actually prevent the action — the **first of the three evaluated
seats** where that property could even be tested, because it is the first where a permission request
reliably arrives for the intents that matter. The documented auto-approve control
(`--allow-all-tools`/`--yolo`/`--allow-all`) is off by default in the registry's invocation and was
proven, by direct A/B capture, to be the exact mechanism that would suppress this gating if it were
ever added to the seat's `start_args`.

The one place this literally fails the OQ's "every tool intent" bar is **reads inside the session's
own working directory** — those complete with zero permission requests, deterministically, across
both captures that exercised a read. Reads *outside* the trusted directory ARE gated (`kind:
"read"`, canonical `rawInput: {path}`). See §4 for what that in-workspace-read gap actually means for
this platform's gate, as directed by the operator amendment.

---

## 2. Candidate adapter (prerequisite check — passed to live-capture)

No separate adapter package exists to identify, unlike `codex-acp`/`pi-acp`. Copilot speaks **native**
ACP: `copilot --acp` is the same compiled binary as the headless `copilot -p "..."` invocation,
started in JSON-RPC-over-stdio server mode instead of one-shot prompt mode.

- **Distribution:** GitHub Copilot CLI, Homebrew cask `copilot-cli` (auto-updating; not an npm/Node
  bridge process).
- **Version actually invoked:** `1.0.83` (self-reported by `copilot --version`), sha256
  `15f218a936f693a6b73df248824b9f7f528c2c61949ff446e4ca6062ee48b084`.
- **Correction to record:** the current registry comment cites `v1.0.75` — stale. The comment update
  in §5 does not need to restate an exact version (see §5.2's rationale for why), but the evidence
  directory pins the actual version tested.

---

## 3. Proof summary

Full reasoning is in `evidence/oq-copilot-acp-001/verdict.md`; condensed here for the record. All six
captures ran under client capabilities `{"fs":{},"terminal":false,"permission":true}` (the harness
explicitly declares it answers permission prompts) and byte-mirror `src/acp_runner.rs:1518-1523`.

| Property | Result | Decisive capture(s) |
|---|---|---|
| (a) every core read/write/edit/bash intent produces a blocking `session/request_permission` with a canonical tool name + `rawInput` compatible with `pretool_payload` | **PARTIAL → FAIL on the literal bar** | edit/execute: `capture-allow` (3/3 gated), `probe-risky` (`rm -rf` gated, not self-approved the way codex's Guardian was), `probe-network` (`curl` gated) — all PASS. read: `capture-allow`/`capture-reject` (in-cwd read never gated — 0/2) — FAIL. `probe-outside-read` (out-of-cwd read IS gated) — PASS for the boundary-relevant case. Identity: every request carries only a free-text `toolCall.title` (e.g. "Create file", "Echo test string"), never a top-level `toolName` or `toolCall.name` — same class of gap as codex's. |
| (b) the auto-approve/default control can be disabled to yield the required property | **PASS** | `--allow-all-tools` (documented, `COPILOT_ALLOW_ALL`) is OFF in the registry's actual invocation; `capture-allow-all-tools` proves adding it collapses `requestPermissionCallCount` from 3 to 0 for the identical turn — the control is real, named, and does exactly what its docs claim |
| (c) a selected reject prevents the action | **PASS** | `capture-reject`: marker file never created, `seed.txt` unchanged, all three gated tool calls resolve `status: "failed"` — the only one of the three evaluated seats where this was even testable |

**Overall for (a):** literally fails the OQ's "every tool intent" bar because of the in-cwd read gap
plus the free-text identity gap. Both gaps are real and independent of each other. Neither is the
"adapter silently proceeds on a risky action" failure mode codex exhibited, nor the "no plumbing
exists" failure mode pi exhibited — see §4 for what the read gap specifically costs.

**Scope of this verdict:** exactly `copilot` CLI `1.0.83` (sha256 `15f218a9...`), invoked as
`copilot --acp` with no extra flags — the registry's exact `start_args`. The CLI auto-updates with no
lockfile pin, so a version bump is **not** covered by this evidence — re-verify before ever flipping
the flag. Triggers that would demand a fresh capture: any change to the default path-trust model
(e.g. in-cwd reads start requiring confirmation, which would only *improve* the verdict), any change
to what `--allow-all-tools` covers, or a change that makes `session/request_permission` carry a
canonical name.

**No `ASSUMPTION[external-transform]` applies** — same reasoning as DES-INPUT-GOV-002 §8 and
DES-INPUT-GOV-003 §3: the ACP `session/request_permission` shape is an in-repo-normalized protocol
carrier, and copilot's ACP mode is native (no bridge translating a second protocol), so there is no
third-party payload-transforming library or service in this picture at all.

---

## 4. Admission calculus — what the in-workspace-read gap actually costs

This section answers the operator amendment's question directly: given every edit/bash-class intent
is genuinely gated, what is the real-world consequence of the one intent class (in-workspace reads)
that is not?

### 4.1 The boundary-equivalence argument

`gate_hook`'s own boundary check treats a `Read` call **inside** the governed unit's assigned
read/write roots as unremarkable: `boundary_denial_with(&roots, &wt, ..., &ctx(src), "Read").is_none()`
(`src/gate_hook.rs`, boundary tests) — i.e. the gate itself would **allow** an in-worktree read, the
same outcome copilot already produces by never asking. Copilot's own trusted-directory model draws
the line in the same place the platform's boundary model does: paths inside the working directory
(≈ the worktree the unit's read/write roots are built from) are pre-authorized; paths outside are not
(`probe-outside-read.ndjson` shows a real, gated, canonical `kind: "read"` request precisely for the
outside case). **For a purely boundary-scoped read policy, admitting copilot today would not change
which reads are allowed versus denied** — both the adapter's own trust model and this platform's gate
draw an equivalent worktree-shaped line, and copilot's line is at least as strict at the boundary
(out-of-worktree reads DO get a client-answerable request, which is the one case boundary policy
actually needs to intercept).

### 4.2 What is NOT equivalent — the two real residual gaps

The boundary-equivalence argument bounds the *allow/deny outcome* gap, not the *governance* gap.
Two things remain genuinely missing, and they are independent of each other:

1. **Missing per-call audit claims for ungated reads.** `evaluate_tool_call` writes the hook-fired
   liveness sentinel and (on the admitted path) a durable `ConformanceClaim` for every call it sees
   (DES-INPUT-GOV-002 §3.4's "equivalence contract"). An in-workspace copilot read never reaches
   `pretool_payload`/`evaluate_tool_call` at all, so **no claim is ever written for it** — not even an
   ALLOW claim. This is invisible today (an unadmitted seat produces no claims for *any* intent), but
   it is exactly what would bite the moment a policy dimension beyond pure worktree-boundary exists —
   e.g. a content- or path-pattern policy ("deny reading `.env`", "flag every read under `secrets/`",
   an audit requirement to log every file a governed unit touched). Nothing shipped in
   `crates/wicked-governance`'s current policy set targets `Read` by content/path-pattern within an
   already-in-boundary path (only the boundary check itself does, and only for out-of-boundary
   paths) — but the policy engine is explicitly **data-driven**, so this is a real, standing gap in
   *audit completeness*, not a hypothetical.
2. **Free-text tool identity** (carried over from §3, restated here because it compounds gap 1): even
   for the intents that ARE gated (edit, execute), a policy keyed on canonical tool identity (e.g.
   "deny Bash", "require review on Write") would see an inconsistent per-call `tool_name` (the
   `toolCall.title` string) rather than a stable name. This is orthogonal to the read gap but shares
   the same root cause — the seat is not admitted, so no work has gone into normalizing what
   `pretool_payload` would see from it.

Neither gap is a security bypass **relative to today's shipped, boundary-only read policy** (§4.1
shows the allow/deny outcome is unchanged); both are governance/observability gaps that matter the
moment richer read policy or canonical-identity policy exists.

### 4.3 The scoped-admission question — filed, not decided

The natural response to §4.1/§4.2 is: admit copilot for the intents it actually gates (edit,
execute) while leaving reads explicitly disclosed-ungoverned, rather than an all-or-nothing verdict
that throws away the two-thirds of the intent space that already works. **That requires a capability
shape change** — `AcpConfig.acp_input_governance` is a single `bool` (DES-INPUT-GOV-002 §3.1,
deliberately chosen as a plain bool for the core-ts wire-safety reason documented there); there is no
per-tool-kind granularity to admit "edit + execute" while declining "read". Introducing one (e.g. a
bitset/enum of gated intent kinds, or a `gate_read: bool` alongside a renamed umbrella flag) is an
architecture decision with its own wire-compatibility and disclosure-semantics consequences (what
does `GovernanceUnenforced` mean for a *partially* admitted seat? does the merge/inheritance logic in
DES-INPUT-GOV-002 §3.6 need a per-kind variant?) — well beyond what a single-seat OQ resolution
should decide unilaterally.

**This design files that question and does not resolve it:**

> **OQ-ACP-GOV-SCOPE-001.** Should `AcpConfig` support a *scoped* (per-tool-intent-kind) admission
> capability — e.g. gate `write`/`edit`/`execute` while explicitly disclosing `read` as
> ungoverned — instead of today's single all-or-nothing `acp_input_governance: bool`? If yes,
> design the shape (representation, wire/core-ts impact, `GovernanceUnenforced` semantics for a
> partial admission, and the DES-INPUT-GOV-002 §3.6 inheritance-on-omission interaction) as its own
> design phase. copilot is the seat that motivates this question today, but the shape decision is
> seat-agnostic and belongs to whoever owns the admission-capability architecture, not to this
> single-seat evidence resolution.

Until that question is answered, copilot's admission stays the all-or-nothing default: **NOT
ADMITTED**, per §1.

---

## 5. Implementation-phase plan (the ONLY production change)

Scope is deliberately minimal and mirrors the codex/pi precedent. **Order matters** (per the
standing operator amendment from the pi run — commit before gates):

1. **Commit EVERYTHING first** — this design doc plus (already committed by the clarify phase)
   `.product/evidence/oq-copilot-acp-001/` — **before** running gates, so the gates run against the
   committed tree. Keep redaction rules intact (no usernames/handles/emails/home paths in committed
   files; raw captures remain only under gitignored `tmp/`).
2. **Edit exactly one site (implementation phase, not this one):** the `copilot` `AcpConfig` block in
   `crates/wicked-council/src/registry.rs` (currently the `// copilot speaks native ACP over stdio
   (\`copilot --acp\`): verified initialize / …` and `// OQ-COPILOT-ACP-001 must prove permission
   coverage before admission.` lines). Replace with the drafted comment in §5.1. **Do not** touch the
   `acp_input_governance: false` line — it is already correct. **Do not** touch any other seat,
   field, or test.
3. **Run gates** (§6).

### 5.1 Proposed replacement comment (implementation phase applies verbatim or near-verbatim)

Replace (`crates/wicked-council/src/registry.rs:286-295`):

```rust
            // copilot speaks native ACP over stdio (`copilot --acp`): verified initialize /
            // session/new / session/prompt with agent_message_chunk streaming (v1.0.75).
            acp: Some(AcpConfig {
                binary: "copilot".into(),
                start_args: vec!["--acp".into()],
                transport: AcpTransport::Stdio,
                auth_method: None,
                // OQ-COPILOT-ACP-001 must prove permission coverage before admission.
                acp_input_governance: false,
            }),
```

with:

```rust
            // Native ACP over stdio (`copilot --acp`, no bridge — same binary as headless mode).
            acp: Some(AcpConfig {
                binary: "copilot".into(),
                start_args: vec!["--acp".into()],
                transport: AcpTransport::Stdio,
                auth_method: None,
                // OQ-COPILOT-ACP-001 resolved NOT admitted (see .product/evidence/
                // oq-copilot-acp-001/ and .product/DES-INPUT-GOV-004-copilot-acp-admission.md).
                // Unlike pi (no permission plumbing) and codex (plumbing exists but is
                // short-circuited by an internal reviewer), copilot's default invocation — this
                // exact one, no extra flags — genuinely blocks on session/request_permission for
                // every edit and every bash-class call observed, including a destructive `rm -rf`
                // and a network `curl`, and a selected reject was proven to actually prevent the
                // action (the only one of pi/codex/copilot where that held). The gap: in-workspace
                // reads complete with zero permission requests (only out-of-workspace reads are
                // gated), and every request carries a free-text title, never a canonical tool
                // name. The read gap does not change worktree-boundary allow/deny outcomes (the
                // gate would allow an in-worktree read anyway) but leaves no audit claim for those
                // calls and is a standing gap the moment a content/path-pattern read policy exists.
                // A scoped (edit/execute-only) admission is possible in principle but needs a
                // capability-shape change beyond a single bool (OQ-ACP-GOV-SCOPE-001, unresolved).
                // Stays disclosed-ungoverned until either the read gap closes or scoped admission
                // ships.
                acp_input_governance: false,
            }),
```

Every runtime claim in that comment is scoped to what the captures actually exercised (four-step
read/edit/bash/write turn, reject turn, `--allow-all-tools` A/B turn, out-of-workspace read probe,
destructive-command probe, network probe) — no extrapolation beyond the pinned artifact.

### 5.2 No test change

The built-in-roster assertion in `registry.rs` (only `claude` carries `acp_input_governance = true`;
`agy`/`codex`/`pi`/`copilot`/`opencode` are unadmitted) already asserts the correct state and passes
unmodified. This design adds no test because it changes no behaviour — only a comment. (If reviewers
want a durable anchor, `verdict.md` + committed evidence + this design doc serve as the regression
record, exactly as the codex/pi precedent.)

---

## 6. Gates (run after committing evidence + design doc, and again after the implementation phase's comment edit)

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test -p wicked-council`
- `cargo test --lib`

**Known-benign failure to expect:** `builtin_floors::tests::floor_fails_closed_outside_a_git_repo`
in `cargo test --lib` fails in this sandbox because `TMPDIR` is set inside the worktree, violating
that test's own "outside any repo" premise. It is a pre-existing environment artifact, unrelated to
this evidence/design work (the clarify phase already confirmed `wicked-council` is 88/88 and `--lib`
is 666/667 with only this one env failure, reproduced in isolation on the same commit). Do not "fix"
it as part of #369.

---

## 7. Forward path (not in scope, recorded for the next attempt)

A governed `copilot` is closer than either `pi` or `codex` — most of the plumbing already works.
Two non-mutually-exclusive preconditions, both needing a fresh capture to a full PASS before the
flag flips under the current all-or-nothing capability:

1. **Close the in-workspace-read gap**, or accept it via §4's scoped-admission path once
   OQ-ACP-GOV-SCOPE-001 is answered. No CLI flag/setting was found in `copilot --help`/`copilot help
   permissions` at this pinned version that forces per-call read confirmation inside the trusted
   directory — closing it may require an upstream request rather than a wicked-side change.
2. **Fix the tool-name identity gap** so `pretool_payload` resolves a canonical name — a wicked-side
   normalization keyed on `toolCall.kind` (a small, stable, already-present enum:
   `read`/`edit`/`execute`) instead of `toolCall.title`, applied ahead of `pretool_payload` for any
   adapter whose requests lack a canonical name (this would also help codex, independently).

Only when a follow-up capture in this same evidence directory shows a blocking
`session/request_permission` for every core intent with a resolvable canonical name — or a scoped
admission capability ships and is deliberately applied to copilot — is flipping
`acp_input_governance` (wholly or partially) justified.
