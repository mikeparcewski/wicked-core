# DES-INPUT-GOV-003 — Resolve OQ-CODEX-ACP-001: codex ACP input-governance admission

**Issue:** wicked-core #367
**Phase:** design (analysis + design + plan). **No production code in this deliverable.**
**Predecessors:**
- DES-INPUT-GOV-001 (recon, #360) — §OQ table defines OQ-CODEX-ACP-001 and its four proof
  properties (blocking request / canonical name+rawInput / reject honoured / disableable
  auto-approve).
- DES-INPUT-GOV-002 (#364) — replaced the Claude-name admission predicate with a per-seat,
  evidence-gated `acp_input_governance` capability that defaults **OFF**. This design resolves one
  seat's gate; it changes no admission mechanism.
- OQ-PI-ACP-001 (#368/#373) — the precedent this mirrors: evidence + verdict + a registry comment
  update, `acp_input_governance` left `false` with no test change.

**Carrier:** `src/acp_permission.rs` (FINDING-062, `pretool_payload` at `:56`) is CLI-agnostic and
unchanged by this work. The decision is about *who is admitted to that carrier*, not the carrier.

**Evidence (the SUBJECT of this design, produced by the capture unit):**
`.product/evidence/oq-codex-acp-001/` — `manifest.md`, `verdict.md`, `README.md` (redaction), five
redacted `*.ndjson` captures, and the `*.mjs` harnesses. Raw captures stay in gitignored `tmp/`.
All committed evidence is REDACTED (no usernames/handles/home paths).

---

## 1. Decision

**codex is NOT ADMITTED. `acp_input_governance` stays `false` for the built-in `codex` seat.**

The decision requires **no field change** — the flag is already `false` (DES-INPUT-GOV-002 set it so
for every non-claude seat). The only production change deferred to the implementation phase is the
**registry comment** on the `codex` `AcpConfig`, updated to cite this verdict and lead with the
headline finding, mirroring the OQ-PI-ACP-001 → `registry.rs` update.

### Headline (why this is worse than absent plumbing)

Unlike `pi-acp` (which simply had no tool-serving permission path), `codex-acp` **ships working
`session/request_permission` machinery and then routes around it by default.** In the decisive
capture (`probe-risky.ndjson`) an explicit `rm -rf` turn shows codex's own internal reviewer
("Guardian") log a concurrent `guardian_assessment` resolving to *"Status: Approved … Risk: medium
… Authorization: high"* — it **observed the action as risky and self-approved it**, deleting the
directory (`exit_code: 0`) with **zero** `session/request_permission` round-trips to the client. An
adapter that sees risk and proceeds without asking is a stronger disqualifier than one with no
plumbing at all. The verdict and the registry comment must both lead with this.

---

## 2. Candidate adapter (prerequisite check — passed to live-capture)

A viable, actively-maintained, pinned candidate exists, so the OQ proceeds on evidence rather than
resolving on "no adapter":

- **Name/version:** `@agentclientprotocol/codex-acp@1.9.0` (npm), `gitHead 67db0d3d4a8a9b4bd3040c4dfdfa0919e9d97be9`.
- **Driving:** `codex-cli 0.153.3`.
- **Provenance:** already the `codex` seat's `AcpConfig.binary = "codex-acp"`; already installed and
  runnable on the evidence host via `wicked-crew`'s own `^1.1.7` dependency.
- **Correction to record:** the current registry comment calls the adapter *Rust*. It is a **TS/Node
  bridge** around the real `codex` CLI. The implementation-phase comment must drop the "Rust" claim.

---

## 3. Proof summary (all three properties FAIL)

Full reasoning is in `evidence/oq-codex-acp-001/verdict.md`; condensed here for the record. All five
captures ran under client capabilities `{"fs":{},"terminal":false,"permission":true}` (the harness
explicitly declares it answers permission prompts) and byte-mirror `src/acp_runner.rs:1521`.

| Property | Result | Decisive capture(s) |
|---|---|---|
| (a) every core read/write/edit/bash intent produces a blocking `session/request_permission` with a canonical tool name + `rawInput` compatible with `pretool_payload` | **FAIL** | `capture-allow` (0 requests, all four intents completed), `capture-readonly` (0 even under `ReadOnly`/`approvalsReviewer:"user"`), `probe-network` (sandbox-denied, never escalated), `probe-risky` (self-approved `rm -rf`) |
| (b) the auto-approve/default control can be disabled to yield the required property | **FAIL** | literal `AskForApproval="never"` is already off (`Agent`/`"on-request"`); the effective gate is `approvalsReviewer:"auto_review"`, a second undocumented-to-the-OQ auto-approver that no reachable built-in mode disables |
| (c) a selected reject prevents the action | **FAIL (untestable)** | `capture-reject` byte-identical to allow; no request ever arrives to reject |

**Secondary compatibility gap (independent of (a)):** even if a request arrived, `presentation.ts`
builds `toolCall` with **no** top-level `toolName` and **no** `toolCall.name` — only a `title` that
is a generic label or the **literal shell command**. `pretool_payload`'s fallback
(`toolName → toolCall.name → toolCall.title`) would key policy on per-call free text, not a canonical
name. A future admission attempt must close this too.

**Scope of this verdict:** exactly `codex-acp@1.9.0` (`gitHead 67db0d3d`) + `codex-cli 0.153.3`.
The owning dependency is a semver range (`^1.1.7`) against a frequently-published package, so a
version bump is **not** covered — re-verify before ever flipping the flag. Triggers that would
demand a fresh capture: a change to `DEFAULT_AGENT_MODE`; a new mode combining
`approvalsReviewer:"user"` with a sandbox that actually grants workspace writes; or routing
`auto_review` decisions back through `session/request_permission` at non-trivial risk.

**No `ASSUMPTION[external-transform]` applies** — same reasoning as DES-INPUT-GOV-002 §8: the ACP
`session/request_permission` shape is an in-repo-normalized protocol carrier, not an external
enrichment/normalization service.

---

## 4. Implementation-phase plan (the ONLY production change)

Scope is deliberately minimal and mirrors the pi precedent. **Order matters** (per operator
amendment — the pi run lost a cycle to an uncommitted worktree):

1. **Commit EVERYTHING first** — the evidence set (`.product/evidence/oq-codex-acp-001/`, currently
   untracked) **and** the registry comment change **before** running gates, so the gates run against
   the committed tree. Keep redaction rules intact (no usernames/handles/home paths in committed
   files; raw captures remain only under gitignored `tmp/`).
2. **Edit exactly one site:** the `codex` `AcpConfig` block in
   `crates/wicked-council/src/registry.rs` (the `// Official ACP-org adapter (…Rust).` line plus the
   `// OQ-CODEX-ACP-001 must prove permission coverage before admission.` line). Replace with the
   drafted comment in §4.1. **Do not** touch the `acp_input_governance: false` line — it is already
   correct. **Do not** touch any other seat, field, or test.
3. **Run gates** (§5).

### 4.1 Proposed replacement comment (implementation applies verbatim or near-verbatim)

Replace:

```rust
            // Official ACP-org adapter (@agentclientprotocol/codex-acp, Rust).
            acp: Some(AcpConfig {
                binary: "codex-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
                auth_method: None,
                // OQ-CODEX-ACP-001 must prove permission coverage before admission.
                acp_input_governance: false,
            }),
```

with:

```rust
            // Official ACP-org adapter (@agentclientprotocol/codex-acp) — a TS/Node bridge
            // around the `codex` CLI, not a Rust binary.
            acp: Some(AcpConfig {
                binary: "codex-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
                auth_method: None,
                // OQ-CODEX-ACP-001 resolved NOT admitted. Five live captures against the pinned
                // codex-acp@1.9.0 (gitHead 67db0d3d) driving codex-cli 0.153.3
                // (see .product/evidence/oq-codex-acp-001/) show the adapter OBSERVE an action's
                // risk and proceed anyway — worse than absent plumbing: an explicit `rm -rf` turn
                // logged codex's own internal reviewer ("Risk: medium, Authorization: high,
                // Approved") and self-deleted the directory with zero session/request_permission
                // round-trips to the client. The permission machinery (CodexApprovalHandler) exists
                // and works, but the default AgentMode's approvalsReviewer:"auto_review" resolves
                // essentially every core read/edit/bash/write intent itself before that machinery is
                // reached — confirmed across ordinary, sandbox-denied, and destructive turns, and
                // even under ReadOnly. Secondary gap: its permission requests carry no canonical
                // tool name, only a human-readable title (often the literal shell command),
                // incompatible with acp_permission::pretool_payload. Stays disclosed-ungoverned
                // until a pinned adapter version proves per-call gating for every core intent.
                acp_input_governance: false,
            }),
```

Every runtime claim in that comment is scoped to what the captures actually exercised (read/edit/
bash/write turn, `ReadOnly` turn, sandbox-denied network turn, destructive `rm -rf` turn) — no
extrapolation beyond the pinned artifacts.

### 4.2 No test change

The built-in-roster assertion in `registry.rs` (only `claude` carries
`acp_input_governance = true`; `codex`/`pi`/`copilot`/`opencode` are unadmitted) already asserts the
correct state and passes unmodified. This design adds no test because it changes no behaviour — only
a comment. (If reviewers want a durable anchor, the `verdict.md` + committed evidence serve as the
regression record, exactly as the pi precedent.)

---

## 5. Gates (run after committing evidence + comment)

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test -p wicked-council`
- `cargo test --lib`

**Known-benign failure to expect:** `floor_fails_closed_outside_a_git_repo` in `cargo test --lib`
fails in this sandbox because `TMPDIR` is set inside the worktree, violating that test's own
"outside any repo" premise. It is a pre-existing environment artifact, unrelated to this
comment-only change (the capture unit already confirmed `wicked-council` is 88/88 and `--lib` is
666/667 with only this one env failure). Do not "fix" it as part of #367.

---

## 6. Forward path (not in scope, recorded for the next attempt)

A governed `codex` is closer than `pi` — the ACP plumbing already exists. Two non-mutually-exclusive
preconditions, both needing a fresh capture to a PASS before the flag flips:

1. Drive codex in a mode whose reviewer is `"user"` **and** whose sandbox actually grants workspace
   writes (no shipped `AgentMode` combines these today) — e.g. an upstream mode addition, or a
   wicked-owned `session/set_session_mode` selection (wired at `CodexAcpServer.ts:1324`) forcing
   `approvalsReviewer:"user"` with an overridden `sandboxPolicy`.
2. Close the tool-name identity gap so `pretool_payload` resolves a canonical name — upstream
   `toolCall.name`/top-level `toolName`, or a wicked-side normalization keyed on `toolCall.kind`
   rather than `title`.

Only when a follow-up capture in this same evidence directory shows a blocking
`session/request_permission` for every core intent with a resolvable canonical name is flipping
`acp_input_governance = true` justified.
