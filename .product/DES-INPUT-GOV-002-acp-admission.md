# DES-INPUT-GOV-002 — Evidence-gated ACP input-governance admission

**Issue:** wicked-core #364
**Phase:** design (analysis + design + plan). **No production code in this deliverable.**
**Predecessor:** DES-INPUT-GOV-001 (recon, #360) — this design implements its §5 first
follow-up: *replace the Claude-name admission predicate with an evidence-gated seat capability*.
**Carrier:** `src/acp_permission.rs` (FINDING-062) is CLI-agnostic and unchanged by this work.

**Operator amendment folded in (this phase):** the live `clis.toml` on real daemons overrides the
`claude` seat **wholesale**, so a capability flag that merely `#[serde(default)]`s to `false` would
**silently disarm** claude's ACP input governance the moment it is added. This design therefore makes
an override that OMITS the capability **inherit the built-in's value** (with an `eprintln!` warning),
exactly as `trust_flags` and `enabled_for_council` already do on key collision (crew#419 / #354),
while an explicit `false` still wins deliberately. Pinned by a merge test in the
`metadata_override_of_a_disabled_builtin_stays_disabled` family. See §3.6.

---

## 1. Problem (what is broken, with the exact site)

`src/acp_runner.rs:3956` builds the per-turn governance context only for a Claude seat:

```rust
let gate_ctx = match (&input.governance, cli_runs_claude(&cli_key)) {
    (Some(g), true) => { /* arm marker, build BoundaryCtx, construct AcpGate inputs */ }
    _ => None,
};
```

`cli_runs_claude` (`src/acp_runner.rs:4490-4495`) classifies every registered non-Claude binary
as non-Claude. So for a governed `codex` / `pi` / `copilot` / `opencode` ACP unit:

- `gate_ctx = None` → no `AcpGate` is constructed (`src/acp_runner.rs:4173-4195`);
- `answer_permission_request` takes its `None` branch and answers with
  `crate::acp_permission::allow_result` instead of `permission_result`
  (`src/acp_runner.rs:2867-2908`);
- that bypasses policy selection, boundary evaluation, the armed/fired markers, and the durable
  `ConformanceClaim` append — the four things that make `fold_input_denial` able to see the gate ran;
- **and it is silent**: the wrapped path emits `GovernanceUnenforced` for the same situation
  (`src/execute_wrapped.rs:748-763`), but the ACP path emits nothing — absence of a wrapped event
  is *not* evidence of enforcement.

**Severity: High** (DES-INPUT-GOV-001 §DEFECT). All four non-Claude seats are built-in,
council-enabled, and carry an `AcpConfig`, and the shared runner explicitly sends governed
non-Claude units down the live ACP session path.

The cause is the **admission predicate**, not the carrier. `acp_permission.rs` already supplies a
carrier-neutral evaluator that writes the identical audit records as the Claude `PreToolUse` hook
(proven by `a_governed_acp_request_is_denied_by_policy_and_recorded`). The fix is to change *who is
admitted to that carrier* and to *disclose* every seat that is not.

---

## 2. Goal & non-goals

**Goal.** Replace the binary-name admission rule with an explicit, per-seat, evidence-gated ACP
input-governance capability that defaults **OFF**. Admit only adapters whose pinned version passed
the DES-INPUT-GOV-001 §3 proof — **today that is `claude` only**. Every unadmitted adapter stays
*explicitly* input-ungoverned: visible on the audit wire via a disclosure event, never silent.
An operator override that omits the flag must not silently disarm a built-in's admission.

**Non-goals (out of scope for this issue).**
- Proving any of `codex-acp` / `pi-acp` / `copilot --acp` / `opencode acp` ready (the OQ-*-ACP-001
  research items in DES-INPUT-GOV-001 §3/§5 — separate research issues). This change makes them
  *admissible once proven*; it admits none of them now.
- Adding bounded-sandbox `trust_flags` for the wrapped fallback of pi/copilot/opencode (separate
  research issues).
- Any change to `acp_permission.rs`, `gate_hook`, the boundary/phase-scope model, or the
  `ConformanceClaim`/fold contract. Those are preserved byte-for-byte on the governed path.

---

## 3. Design

### 3.1 The capability: `acp_input_governance` on `AcpConfig` (default `false`)

Add one field to `wicked_council::types::AcpConfig` (the **resolved** type — a clean `bool`):

```rust
/// Whether this ACP adapter has PASSED the evidence proof (DES-INPUT-GOV-001 §3) that lets the
/// engine drive its `session/request_permission` requests through the shared policy+audit gate.
/// Default `false`: an adapter is input-UNGOVERNED on ACP until its pinned version is proven to
/// (a) block every tool action on a permission request carrying canonical name + raw input,
/// (b) honour a reject, and (c) have its auto-approve/default-permission surface disabled.
/// This is admission to `AcpGate` — NOT a claim that the seat is sandboxed.
#[serde(default)]
pub acp_input_governance: bool,
```

Placement and shape rationale:

- **On `AcpConfig`, not `AgenticCli`.** The proof is pinned to a *specific ACP adapter binary and
  version* (`claude-agent-acp`), which is exactly what `AcpConfig` describes. Keeping the flag next
  to `AcpConfig::binary` is what lets §3.6 refuse to inherit the proof when an override swaps the
  adapter binary — a top-level flag divorced from the binary could not make that distinction. A
  seat with no `AcpConfig` has no ACP path to govern, so the flag would be meaningless on the parent.
- **`bool` with `#[serde(default)]`, not `Option<bool>` — on the RESOLVED type.** `AgenticCli`
  (and therefore its nested `AcpConfig`) crosses the core-ts boundary: `crates/wicked-core-ts/src/lib.rs`
  deserializes a JSON array of `AgenticCli` from `clisJson` (`:57`, `:646`). The CLAUDE.md wire gotcha
  applies: an `Option` without `skip_serializing_if` serializes as `null`, so TS `=== undefined`
  guards would be dead. A plain `bool` defaulting to `false` is always present on the wire and reads
  "absent ⇒ unadmitted", the fail-safe direction. **The omission-vs-explicit distinction the amendment
  needs lives only in the TOML parse layer (§3.6), never on the resolved wire type.**
- **Default OFF is the whole safety argument.** DES-INPUT-GOV-001 §4.3: a default-deny capability
  flag is safer than broadening a binary-name predicate — a new seat, a typo, or a fresh user record
  defaults to *disclosed-ungoverned* rather than *silently-governed-but-not-really*.

**Built-in roster change** (`crates/wicked-council/src/registry.rs`, the six `AcpConfig { … }`
literals at `:100,132,176,202,229,255`): set `acp_input_governance: true` on the **`claude`** record
only. Set it explicitly `false` on `agy`, `codex`, `pi`, `copilot`, `opencode`, each with a comment
citing the seat's OQ-*-ACP-001 research gate. (Struct literals must name every field; do not rely on
the serde default here — being explicit is the audit trail for *why* each seat is or isn't admitted.)

### 3.2 The admission predicate (the `3956` fix)

Replace `cli_runs_claude` at the call site with a capability read against the **merged** registry:

```rust
/// Whether this seat's ACP adapter is ADMITTED to input governance — i.e. its pinned version has
/// passed the evidence proof (DES-INPUT-GOV-001 §3) and the MERGED registry records
/// `acp.acp_input_governance = true`. Reads `registry_record` (built-ins ∪ user overlay, with the
/// §3.6 inheritance already applied), the same loader `acp_config_for` uses, so a user overlay
/// decides admission exactly as it decides transport. Absent record, absent acp, or `false`
/// ⇒ NOT admitted (fail-safe).
fn cli_acp_input_governed(cli_key: &str) -> bool {
    registry_record(cli_key)
        .and_then(|c| c.acp)
        .map(|a| a.acp_input_governance)
        .unwrap_or(false)
}
```

Call site (`src/acp_runner.rs:3956`) becomes:

```rust
let gate_ctx = match (&input.governance, cli_acp_input_governed(&cli_key)) {
    (Some(g), true) => { /* UNCHANGED body: arm marker, build BoundaryCtx, AcpGate inputs */ }
    (Some(_g), false) => {
        // Governed unit on an UNADMITTED ACP adapter. Disclose, then fall through ungoverned —
        // see §3.3. Do NOT fail the unit (that would take out every seat until adapters are
        // proven); do NOT silently allow (that is the defect).
        None
    }
    (None, _) => None,
};
```

Everything inside the `(Some(g), true)` arm — the armed-marker write and fail-closed-on-arm-error,
the `BoundaryCtx` construction (write/read roots, home, `claude_config_dir`, `pre_build_scope`), and
the tuple returned into the per-turn `AcpGate` build (`src/acp_runner.rs:4173-4195`) — is **preserved
unchanged**. Only the predicate feeding the `match` changes.

`cli_runs_claude` is no longer the governance admission rule. Grep shows `3956` is its only
governance use; if it becomes dead after this change, remove it and keep `binary_is_claude`
(`src/execute_wrapped.rs:212`, still used by the wrapped path at `:685`).

### 3.3 Explicit disclosure (the "never silent" requirement)

Today a governed non-Claude ACP turn is silent. DES-INPUT-GOV-001 §4.1 requires equivalent
disclosure to the wrapped path. **Reuse `CoreEvent::GovernanceUnenforced`** (`src/event.rs:532`) —
it already carries `{ session, ord, attempt, cli, reason }` and already means exactly "this governed
unit ran without input governance." Emitting the same event from both carriers keeps one audit
vocabulary and lets existing consumers treat ACP and wrapped identically.

Emit it in the `(Some(_g), false)` arm, before the shared session path runs:

```
cli:    cli_key (the seat).  [Match the wrapped path's "argv[0]" intent as closely as the ACP
        context allows; document the choice at the call site.]
reason: "unit is governed but the ACP adapter for '<cli_key>' is not admitted to input governance
         (acp_input_governance=false); its tool calls are answered by allow_result, unchecked —
         see DES-INPUT-GOV-001 OQ-<SEAT>-ACP-001"
```

Guard it exactly as the wrapped path guards its emission: only emit when there is a real seat/adapter
to name (never an empty string), so the event is always actionable.

The `answer_permission_request` `None` branch (`allow_result`) is unchanged: an unadmitted governed
turn is *disclosed* as ungoverned and still answered permissively (fail-closed only on malformed
requests, which `allow_result` already does). The disclosure is what turns "silent bypass" into
"explicit, on-the-wire ungoverned posture."

### 3.4 What is preserved (the equivalence contract)

- `AcpGate` construction, boundary evaluation order (boundary → phase-scope → policy), the armed
  marker, the hook-fired sentinel, and the `ConformanceClaim` append — all inside the untouched
  `(Some(g), true)` arm and inside `acp_permission::permission_result`.
- The Claude carrier's evidence shape. An admitted adapter denied by policy produces the *same*
  decisions-log records as the Claude `PreToolUse` hook — already asserted carrier-neutrally by
  `acp_permission::tests::a_governed_acp_request_is_denied_by_policy_and_recorded`.
- Fail-closed at the protocol edge (`pretool_payload` → `None` ⇒ `cancelled`, missing option kind ⇒
  `cancelled`) — unchanged in `acp_permission.rs`.

### 3.5 (superseded) — see §3.6

The clarify draft proposed a documentation-only "operators must restate the flag" migration note.
The operator amendment supersedes it: documentation is not enough because a live daemon already
overrides `claude` wholesale. The mechanism below replaces the note.

### 3.6 Inheritance on omission (the operator amendment — the disarm fix)

**The hazard.** `load()` merges a user TOML over the built-ins; on key collision the user record
replaces the built-in **wholesale** (proven by `load_merges_user_toml_acp_config`: an override that
restates `[cli.acp]` replaces the built-in's ACP table entirely). The live `clis.toml` overrides
`claude` wholesale. When this capability ships, that override's `[cli.acp]` will not mention
`acp_input_governance`, so a naive `#[serde(default)] = false` resolves it to `false` →
**claude's ACP input governance is silently disarmed on real daemons.**

**The fix — mirror the existing `trust_flags`/`enabled_for_council` precedent** (`registry.rs`
`load()`), which captures whether the override *omitted* a field (vs. specified it, including an
explicit empty/`false`) **before** the `From<TomlCli>` conversion, and on key collision inherits the
built-in's value with an `eprintln!` warning. An explicit value always wins.

Because the flag is nested inside `[cli.acp]`, the TOML parse layer must preserve the
omission bit that a plain `bool` would erase. Introduce a TOML-only mirror of `AcpConfig`:

```rust
/// TOML-parse mirror of `AcpConfig`. Distinct from `AcpConfig` so `acp_input_governance` can be
/// `Option<bool>` HERE (None = omitted ⇒ inherit on collision; Some(_) = explicit ⇒ wins) while the
/// resolved `AcpConfig` stays a clean `bool` on the core-ts wire (§3.1).
#[derive(Debug, Deserialize)]
struct TomlAcpConfig {
    binary: String,
    #[serde(default)] start_args: Vec<String>,
    #[serde(default)] transport: AcpTransport,
    #[serde(default)] auth_method: Option<String>,
    #[serde(default)] acp_input_governance: Option<bool>,
}

impl From<TomlAcpConfig> for AcpConfig {
    fn from(t: TomlAcpConfig) -> Self {
        AcpConfig {
            binary: t.binary,
            start_args: t.start_args,
            transport: t.transport,
            auth_method: t.auth_method,
            // Resolved default when there is nothing to inherit; load() overwrites this with the
            // built-in's value when the override OMITTED the flag AND the built-in adapter matches.
            acp_input_governance: t.acp_input_governance.unwrap_or(false),
        }
    }
}
```

`TomlCli.acp` changes from `Option<AcpConfig>` to `Option<TomlAcpConfig>`, and `From<TomlCli>` maps
it with `t.acp.map(Into::into)`. In `load()`, alongside `omitted_trust` / `omitted_enabled`:

```rust
// Whether the override supplied a [cli.acp] table that OMITTED acp_input_governance. Only then do
// we inherit — an explicit Some(true)/Some(false) is the operator's deliberate choice and wins.
let omitted_acp_gov = tcli
    .acp
    .as_ref()
    .is_some_and(|a| a.acp_input_governance.is_none());
```

Then, inside the key-collision branch (after `slot` is found), before `*slot = cli;`:

```rust
if omitted_acp_gov {
    if let (Some(new_acp), Some(builtin_acp)) = (cli.acp.as_mut(), slot.acp.as_ref()) {
        // Inherit the PROOF only when the override kept the SAME adapter binary. An override that
        // swaps to a different, UNPROVEN adapter must NOT inherit admission — it stays
        // disclosed-ungoverned (fail-safe), which is the whole point of "admit only proven adapters".
        if builtin_acp.acp_input_governance
            && !new_acp.acp_input_governance
            && new_acp.binary == builtin_acp.binary
        {
            new_acp.acp_input_governance = true;
            eprintln!(
                "wicked-council: seat '{}' overrides a built-in whose ACP adapter '{}' is admitted \
                 to input governance but the override's [cli.acp] omits acp_input_governance — \
                 inheriting `true` (specify `acp_input_governance = false` to run it ACP-ungoverned \
                 deliberately; wicked-core#364)",
                cli.key, builtin_acp.binary
            );
        }
    }
}
```

Resulting truth table (override collides with a built-in that is admitted, e.g. `claude`):

| Override `[cli.acp]` | adapter binary | resolved `acp_input_governance` |
|---|---|---|
| omits the flag | same as built-in | **`true`** (inherited + warned) |
| omits the flag | swapped/unproven | `false` (disclosed-ungoverned, fail-safe) |
| `acp_input_governance = false` | any | `false` (explicit — wins) |
| `acp_input_governance = true` | any | `true` (explicit — wins) |
| no `[cli.acp]` at all | — | `acp = None` ⇒ ACP stripped; claude runs the **wrapped** path, still governed by the PreToolUse hook (no disarm) |

This is the exact behaviour the amendment asked for (inherit-on-omit + warning + explicit-wins),
strengthened by the adapter-binary condition so inheritance can never admit an unproven adapter.

---

## 4. Acceptance criteria (SMART + testable)

- **AC-1 (admission is capability-gated).** `cli_acp_input_governed("claude") == true`;
  `cli_acp_input_governed` is `false` for each of `codex`, `pi`, `copilot`, `opencode` from the
  built-in registry. *(unit test on the predicate + built-in roster)*
- **AC-2 (user overlay admits a proven adapter explicitly).** A user TOML record with
  `[cli.acp] … acp_input_governance = true` makes `cli_acp_input_governed(key) == true`; an explicit
  `false` makes it `false`. *(registry deser + predicate test)*
- **AC-3 (governed + admitted → same evidence as Claude PreToolUse).** A policy-denied governed ACP
  call on an admitted adapter is rejected (`optionId == reject`) AND leaves a `ConformanceClaim`
  naming the denying policy, the tool-call annotation, and the phase liveness sentinel — identical to
  the wrapped hook. *(covered today by
  `acp_permission::tests::a_governed_acp_request_is_denied_by_policy_and_recorded`; keep green, cite
  as the equivalence proof)*
- **AC-4 (malformed fails closed).** An unparseable/tool-name-less permission request yields
  `cancelled`, never a guessed allow, on the governed path. *(covered by
  `acp_permission::tests::a_request_without_a_tool_name_is_not_evaluable` + the `permission_result`
  unparseable branch)*
- **AC-5 (unadmitted posture is explicit).** A governed unit routed to an unadmitted ACP seat emits
  `CoreEvent::GovernanceUnenforced` with a non-empty `cli` and a reason naming the seat and the
  admission gate — and produces no `AcpGate` (`governed == false`). *(new test — §5)*
- **AC-6 (no regression on the untouched arm).** The `(Some(g), true)` arm still writes the armed
  marker, builds the boundary from unit cwd + extra roots, and reports `governed: gate.is_some()`.
  The `governed:` hardcode guard (`src/acp_runner.rs:6451`) stays green.
- **AC-7 (override omitting the flag inherits admission — the disarm fix).** A user TOML that
  overrides `claude` wholesale, restates `[cli.acp]` with the **same** adapter binary, and omits
  `acp_input_governance`, resolves to `acp_input_governance == true` after `load()`. An explicit
  `false` in that same override resolves to `false`. *(merge test, `metadata_override_…` family)*
- **AC-8 (swapped adapter does not inherit the proof).** The same override but with a **different**
  `[cli.acp].binary` and the flag omitted resolves to `false`. *(merge test)*

---

## 5. Test plan (what the build phase must add)

Framework: `cargo test --lib` (root `wicked-core`) + `cargo test -p wicked-council` (the field +
merge logic live there). No network, deterministic, temp-dir scoped — matching the existing
`acp_permission` / `acp_runner` / `registry` test idioms.

1. **Predicate + roster (AC-1).** New `#[cfg(test)]` near `registry_record` in `acp_runner.rs`:
   assert `cli_acp_input_governed` is `true` for `claude`, `false` for the other four built-ins.
   Drives the merged loader, so it also proves the built-in roster wiring.
2. **Built-in roster (AC-1, wicked-council).** In `registry.rs` tests: only the `claude` built-in
   `AcpConfig` has `acp_input_governance == true`; all others `false`. Iterate `builtin()` so a new
   admitted seat cannot be added without updating the assertion.
3. **Explicit overlay admission (AC-2).** `registry.rs` test: a TOML `[cli.acp] acp_input_governance
   = true` round-trips to `AcpConfig { acp_input_governance: true }`; `= false` → `false`.
4. **Inheritance on omission (AC-7) + explicit-false-wins.** `registry.rs` merge test in the
   `metadata_override_of_a_disabled_builtin_stays_disabled` style: override `claude` wholesale with a
   restated `[cli.acp]` (same `binary`) that omits the flag → merged `true`; a second variant with an
   explicit `acp_input_governance = false` → merged `false`. Assert against `builtin()`'s claude acp
   value so the test tracks whatever the built-in posture is.
5. **Swapped adapter does not inherit (AC-8).** `registry.rs` merge test: override `claude` with
   `[cli.acp].binary = "rogue-acp"` and the flag omitted → merged `false`.
6. **Equivalence + fail-closed (AC-3/AC-4).** Keep and cite the existing carrier-neutral
   `acp_permission` tests — they already prove denied→reject+durable-claim and malformed→cancelled
   for *any* `AcpGate`, which is what an admitted adapter gets. Add a comment linking them to #364.
7. **Unadmitted disclosure (AC-5).** New runner-level test: for a governed unit on an unadmitted
   seat, assert (a) `AcpGate` is absent (`governed == false` on the `StepOutput`) and (b) a
   `GovernanceUnenforced` event was emitted with a non-empty `cli` and the admission-gate reason.
   Use the existing event-capture harness in `acp_runner.rs` tests. If driving a full turn offline is
   too heavy, factor the disclosure decision into a small helper so the `(Some(_g), false)` branch +
   event are unit-testable without a live ACP process (mirrors how the wrapped `(Some(_), false)` arm
   is reasoned about).
8. **`governed:` hardcode guard (AC-6).** The existing source-scanning guard at
   `src/acp_runner.rs:6451` stays; confirm the edited arm still returns `governed: gate.is_some()`.

---

## 6. Gates (definition of done for the build phase)

- `cargo test --lib` — green (root `wicked-core` crate).
- `cargo test -p wicked-council` — green (the `AcpConfig` field, `TomlAcpConfig`, and merge logic).
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- **core-ts caveat (CLAUDE.md, and it applies here).** `AgenticCli` (hence nested `AcpConfig`) is
  deserialized by `crates/wicked-core-ts` from `clisJson`. The new field is an additive `bool` with
  `#[serde(default)]`, so TS rosters that omit it deserialize to `false` (safe) and Rust-serialized
  rosters always include a bool (never `null`). Per the standing rule, after the change run
  `cd crates/wicked-core-ts && cargo test`. Do not fold core-ts into the workspace build.
- `CHANGELOG.md` entry: note the admission-model change (binary-name → capability), the default-OFF
  posture, and the inherit-on-omit behaviour for overrides (with the explicit-`false` opt-out).

---

## 7. Risks & mitigations

- **R1 — live `claude` override silently disarmed.** *Resolved by §3.6* (inherit-on-omit with the
  same-binary condition + warning). This is the amendment's core requirement and the reason the doc
  moved from a documentation note to a merge-level mechanism.
- **R2 — an override swaps `[cli.acp].binary` to an unproven adapter and expects governance.** The
  same-binary condition makes it resolve to `false` (disclosed-ungoverned), and the absence of the
  inheritance warning is itself the signal. Admitting the swapped adapter takes a deliberate explicit
  `acp_input_governance = true` — reviewable, and a documented red flag per the field doc-comment.
- **R3 — someone sets `acp_input_governance = true` on an unproven adapter.** The flag *is* the
  admission rule, so this routes that adapter through the gate. Mitigation is documentary + process:
  the field doc-comment and the OQ-*-ACP-001 issues state the proof is a precondition; the flag name
  makes an unproven `true` a reviewable red flag. (A hardcoded per-CLI allowlist was rejected — it
  would reintroduce the exact anti-pattern this change removes.)
- **R4 — disclosure event spam.** Emitted once per governed-unit turn on an unadmitted seat, same
  cadence as the wrapped `GovernanceUnenforced`; acceptable and symmetric.
- **R5 — `cli_runs_claude` left dead.** Verify usages after the edit; remove if `3956` was its only
  governance caller, keeping `binary_is_claude` (still used by the wrapped path).

---

## 8. External-transform convention

This design relies on no third-party library or service that transforms a payload. The ACP bridge's
`session/request_permission` shape is a *protocol carrier* already normalized in-repo by
`acp_permission::pretool_payload`, not an external enrichment/normalization service. Therefore no
`ASSUMPTION[external-transform]` entries apply — consistent with DES-INPUT-GOV-001 §6.

---

## 9. Summary of the concrete edits the build phase will make

1. `crates/wicked-council/src/types.rs` — add `#[serde(default)] pub acp_input_governance: bool` to
   `AcpConfig` (doc-comment from §3.1).
2. `crates/wicked-council/src/registry.rs`:
   - add `struct TomlAcpConfig` + `From<TomlAcpConfig> for AcpConfig` (§3.6);
   - change `TomlCli.acp` to `Option<TomlAcpConfig>` and map it in `From<TomlCli>`;
   - set `acp_input_governance: true` on the `claude` built-in `AcpConfig`, explicit `false`
     (commented with the OQ gate) on the other five;
   - add the `omitted_acp_gov` capture + collision inheritance block with the `eprintln!` warning.
3. `src/acp_runner.rs` — add `cli_acp_input_governed`; change the `3956` predicate; add the
   `(Some(_g), false)` disclosure arm emitting `GovernanceUnenforced`; leave the `(Some(g), true)`
   body untouched; retire `cli_runs_claude` if now unused (keep `binary_is_claude`).
4. Tests per §5; `CHANGELOG.md` entry per §6.
