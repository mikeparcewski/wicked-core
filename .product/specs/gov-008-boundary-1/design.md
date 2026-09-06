# DESIGN — GOV-008 Boundary 1: CLI-agnostic OS-sandbox DENY floor around the CLI worker spawn

**Umbrella:** wicked-core #360. **Source design:** `.product/DES-INPUT-GOV-008-os-sandbox-and-governed-mcp.md` §3.
**Baseline (consumed):** `.product/specs/gov-008-boundary-1/clarify.md` (prior phase). This document REFINES that plan into an implementation-ready design; it does not re-solve it. Read the clarify doc first — §1 (source anchoring), §4 (module + wrap points), §5 (fail-closed), §6 (default-OFF flag) are assumed here and only amended where the operator note changes them.
**Phase:** DESIGN. Analysis + design only. **No production code is written or committed in this phase.** Every code block is a *proposed* shape for the implementation phase, not a change made now.

**SCOPE (unchanged, restated so the impl phase cannot drift):** WRITE-containment floor ONLY. Not exfiltration protection, not a read jail, not egress/DLP (that is 008b). The model-API network channel stays OPEN by necessity. Comments in the shipped code must say so.

---

## 0. Operator amendments folded in (the two deltas vs. clarify)

The clarify deliverable was accepted with two additions, now first-class design elements:

- **A1 — the fail-closed DEGRADED path must DISCLOSE OBSERVABLY.** When a unit runs unsandboxed because no launcher is present, OR because only `firejail` is present (which is NOT `Sandboxed` for a *worker* — it denies only network, which we deliberately keep open), the design MUST emit a real wire/audit signal an operator can see, mirroring the `GovernanceUnenforced` disclosure pattern. §2 specifies a new `SandboxUnenforced` `CoreEvent`. A test asserts it fires (§5, T5).
- **A2 — write-root is the SINGLE source shared with `armed_write_roots`.** Already the clarify design; now backed by an explicit regression test that a write to an allowed `extra_write_root` (the estate graph dir) is NOT kernel-killed — the core#217 regression (§5, T3).

Everything else in clarify stands: `NetworkPolicy::Allow` for workers (§1), default-OFF flag (§4), commit-before-gates (§6), and the kernel-denied (EPERM + file-absent) test as the acceptance keystone (§5, T1).

---

## 1. Network policy — the load-bearing divergence (restated, locked)

The generalized profile builder takes an explicit `NetworkPolicy`:

- `NetworkPolicy::Deny` — validator scripts. Emits SBPL `(deny network*)` / bwrap `--unshare-net`. **Byte-identical to today** for existing validator callers.
- `NetworkPolicy::Allow` — CLI workers. Emits **no** network rule; the worker reaches its model host. This is why a worker can never be dressed as network-contained, and why `firejail` (network-only) buys a worker nothing (A1).

Filesystem containment is independent of this choice. Tightening worker network is 008b / OQ-SANDBOX-NET-001, explicitly out of scope.

---

## 2. The `SandboxUnenforced` disclosure event (A1) — full wiring

Modelled on `GovernanceUnenforced` (`event.rs:533`), which is the established "a unit ran without a governance layer, loudly disclosed, never silent" precedent (ORCHESTRATOR.md §10). Boundary 1's degraded path is the write-containment analogue.

### 2.1 Rust enum variant (`src/event.rs`, proposed)

```rust
/// (Boundary 1 / DES-INPUT-GOV-008 §3, §5) A CLI worker spawned WITHOUT the kernel
/// write-containment floor armed, on a run that requested it (the default-OFF `os_sandbox`
/// capability was ON). Fires at spawn time on either carrier when the applied level is below
/// `Sandboxed` for WRITE containment:
///  - `BestEffort`: no sandbox tool on PATH (all of Windows; a host without sandbox-exec/bwrap).
///  - `NetworkOnly` (firejail): denies network only — for a WORKER that is not write-containment,
///    and worker network is deliberately open anyway, so it is disclosed as write-uncontained.
///  - a degraded arm: a supported tool was present but the profile could not be built
///    (e.g. the worktree root failed to canonicalize) and the spawn proceeded uncontained.
/// This is the WRITE-containment sibling of `GovernanceUnenforced`; it is NOT an exfiltration or
/// audit claim. A unit running without the deny floor is never silent.
SandboxUnenforced {
    session: String,
    ord: u32,
    attempt: u32,
    /// Which CLI ran uncontained — per carrier, same convention as `GovernanceUnenforced`:
    /// the wrapped path emits `argv[0]`; the ACP path emits the registry seat key.
    cli: String,
    /// The level ACTUALLY applied (`BestEffort` / `NetworkOnly`), lower-cased on the wire.
    level: String,
    /// Human-readable why: "no OS-sandbox tool on PATH", "firejail is network-only (no write
    /// containment for a worker)", "worktree root <p> failed to canonicalize; arming skipped".
    reason: String,
},
```

`level` is carried as a `String` (the lower-cased `SandboxLevel` — `"best-effort"` / `"network-only"`) rather than embedding the enum, to keep the wire shape flat and match how other events stringify small enums. The impl phase adds a `SandboxLevel::as_wire(&self) -> &'static str` helper (or reuses an existing spelling if one exists).

### 2.2 JSON serialization (`src/event.rs` `to_json`, proposed)

```rust
CoreEvent::SandboxUnenforced { session, ord, attempt, cli, level, reason } => json!({
    "type": "sandboxUnenforced",
    "session": session,
    "ord": ord,
    "attempt": attempt,
    "cli": cli,
    "level": level,
    "reason": reason,
}),
```

### 2.3 core-ts mirror (public-type change ⇒ core-ts gate applies)

Adding a `CoreEvent` variant is a public-type change, so per CLAUDE.md the impl phase MUST:
- mirror the variant in `crates/wicked-core-ts/src/lib.rs` (the `check(...)` block near L2560 that asserts `type` + field names for each event — add a `SandboxUnenforced` case with `["type","session","ord","attempt","cli","level","reason"]`);
- regenerate / hand-update `crates/wicked-core-ts/index.d.ts` if events are surfaced there;
- run `cd crates/wicked-core-ts && cargo test` **separately** (workspace-excluded on purpose — `cargo test --workspace` never touches it).
- **Wire-shape guard (CLAUDE.md):** every field is a required scalar (`String`/`u32`), no `Option`, so there is no `null`-vs-absent trap; TS consumers see all fields always present.

### 2.4 Emit points

- **Wrapped path** (`execute_wrapped.rs`, at the spawn site ~L1937, immediately before/after building the wrapped `Command`): when `os_sandbox` is ON and `detect_worker_sandbox(...)` returns `level != Sandboxed`, `self.emit_event(CoreEvent::SandboxUnenforced { cli: argv[0].clone(), level, reason, .. })`. This is the same `emit_event` surface the adjacent `GovernanceUnenforced` (L752) already uses.
- **ACP path** (`acp_runner.rs`, in/around `start_acp_process` at the spawn decision): same, with `cli = cli_key` (the registry seat key — the ACP-path convention `GovernanceUnenforced` already follows at L4284/L4618). Because the ACP session is cached and reused, the event fires ONCE per spawn (not per turn), matching the "wrap at spawn" lifetime.

**When it does NOT fire:** (a) `os_sandbox` OFF (default) — the feature was not requested, nothing to disclose; (b) `level == Sandboxed` — the floor armed, no gap. This mirrors `GovernanceUnenforced`'s "only when there is something real to disclose" discipline (it is suppressed when `argv` is empty / nothing ran).

---

## 3. Module & wrap-point design (refines clarify §4)

### 3.1 Surface (proposed)

```rust
// PROPOSED — not built this phase.
#[derive(Clone, Copy)]
enum NetworkPolicy { Deny, Allow }

struct WorkerSandbox {
    /// Prependable wrapper argv, ending in `sandbox-exec -p <profile>` or `bwrap … --`.
    /// EMPTY ⇒ no wrap (floor); pair with `level` to decide disclosure.
    wrapper: Vec<String>,
    level: SandboxLevel,
    /// Why we are below `Sandboxed`, when we are — feeds `SandboxUnenforced.reason`. None when Sandboxed.
    downgrade_reason: Option<String>,
}

/// write_roots[0] = primary (worktree/cwd); the rest = estate graph dir + any launcher extras.
/// The SAME slice `armed_write_roots` derives (A2) — never re-derived independently.
fn detect_worker_sandbox(write_roots: &[PathBuf], net: NetworkPolicy) -> WorkerSandbox
```

- `macos_sandbox_profile` generalizes its single `extra_write: Option<&Path>` to `write_roots: &[Path]` (an `allow file-write* (subpath …)` per canonicalizable root) plus a `net: NetworkPolicy` gate on the `(deny network*)` line. The **primary** root failing to canonicalize ⇒ return no profile ⇒ `WorkerSandbox` degrades with a `downgrade_reason` (fail-closed narrow, disclosed). A non-primary root failing to canonicalize is dropped from the writable set (still fail-closed narrow; the run may then hit a kernel deny on that dir, which is safe, not an escape).
- The bwrap branch generalizes identically: `--bind` each write-root (bound LAST so it wins over the secret-dir tmpfs masks), `--unshare-net` only under `Deny`.
- Existing validator callers are preserved by calling the generalized builder with `[run_dir, coverage_dir]` + `NetworkPolicy::Deny`. A golden test (§5, T6) pins byte-identical output.

### 3.2 Wrap points (unchanged from clarify, restated for the impl phase)

- **Wrapped:** `execute_wrapped.rs:~1937` `Command::new(&argv[0])`. When armed, become `Command::new(<wrapper-abs-path>)` + `[wrapper-tail…, argv[0], argv[1..]…]`. `.hardened()`, `redirect_scratch_into_boundary`, `gov_env`, `current_dir(cwd)`, pipes all still applied to the (now wrapper) `cmd`; env passes through the wrapper to the real child.
- **ACP:** `acp_runner.rs` `start_acp_process` `build_cmd(binary)` closure (~L1408). Wrap the real binary; the Windows `.cmd` retry (~L1487) re-invokes `build_cmd` and MUST wrap the *real* binary name, not the wrapper. `process_group(0)`, `worker_config_dir`, `acp_governance_env`, `scratch_tmp`, piped stdio preserved. The reaper tracks the wrapper pid (ppid-linked to us) — verify at impl.

**Absolute wrapper path required** (`find_on_path` returns abs): `.hardened()` may reset the child `PATH`, so a bare wrapper name could fail to resolve.

### 3.3 Write-root single source (A2)

Feed `detect_worker_sandbox` the SAME `(cwd, extra_write_roots, graph_dir)` inputs that `armed_write_roots` (`execute_wrapped.rs:1027`) turns into `WICKED_WRITE_ROOTS`. Kernel-writable set ≡ gate-hook-advertised set. The impl phase should route both through one helper so a future change to the write-root list cannot desync the two (the classic two-definitions bug). The ACP path must derive the same set for its `cwd` (+ graph dir when applicable).

---

## 4. Default-OFF rollout flag (refines clarify §6)

- Add a `#[serde(default)]` **plain `bool`** capability (working name `os_sandbox`) on the seat / `AcpConfig` (and/or the governance context for the wrapped path), **default `false`** = today's behaviour, byte-unchanged. `true` = arm Boundary 1.
- Inherit-on-omit merge semantics per the `acp_governance_env` / DES-002 §3.6 precedent.
- **core-ts caveat:** a new `AcpConfig`/`AgenticCli` field is deserialized by core-ts — plain `bool` + `#[serde(default)]`, never a bare `Option`; run `cd crates/wicked-core-ts && cargo test`.
- Interaction with A1: `SandboxUnenforced` fires only when the flag is ON and the level degraded. Flag OFF ⇒ no wrap, no disclosure (the operator did not ask for the floor).

---

## 5. Test matrix (the acceptance the impl phase must satisfy)

`#[cfg(unix)]` kernel tests **assert-skip cleanly** when no real sandbox tool is on PATH (CI without `bwrap`/`sandbox-exec` stays green but honest). Keystone = T1.

| # | Test | Asserts | Maps to |
|---|---|---|---|
| **T1** | Write OUTSIDE worktree is KERNEL-DENIED | Arm over a temp worktree; child does `echo x > <sibling-outside>/pwned`; assert file **absent** AND child saw EPERM / "Operation not permitted". Proves the *kernel* denied — NOT merely that argv contains `sandbox-exec`. (A wrapper-argv-only assertion is explicitly INSUFFICIENT as sole coverage.) | task keystone |
| **T2** | Write INSIDE worktree succeeds | Same arming; child writes `<worktree>/ok`; assert it lands. Guards over-tight profile. | task |
| **T3** | Write to allowed `extra_write_root` (estate graph dir) is NOT kernel-killed | Arm with `write_roots = [worktree, graph_dir]`; child writes `<graph_dir>/x` (+ a `-wal`-style sibling); assert both land. The core#217 regression. | **A2** |
| **T4** | Wrap covers BOTH paths | wrapped-spawn surface + `start_acp_process` each produce a wrapped `Command` (abs wrapper prepended, real binary+args preserved, `.cmd` retry wraps the real name, `current_dir`/env/pipes intact). At least one path driven end-to-end into T1 so the wrap is load-bearing, not cosmetic. | task |
| **T5** | `SandboxUnenforced` fires on the degraded path | Force no-launcher (or firejail-only, or forced primary-root canonicalize failure) with the flag ON; assert a `SandboxUnenforced` event with the right `cli`/`level`/`reason` is emitted AND the spawn did **not** claim `Sandboxed`. Also assert it does **NOT** fire when `level == Sandboxed` or the flag is OFF. | **A1** |
| **T6** | Validator regression (byte-identical) | The generalized builder called `NetworkPolicy::Deny` + `[run_dir, coverage_dir]` reproduces the pre-refactor SBPL/bwrap argv golden — validator net-deny + single run-dir unchanged. | safety |
| **T7** | Worker profile keeps network OPEN | A worker profile (`NetworkPolicy::Allow`) contains **no** `(deny network*)` / no `--unshare-net`; a validator profile still does. | clarify §1 |
| **T8** | core-ts event mirror | `crates/wicked-core-ts` `check(...)` asserts `sandboxUnenforced` + its field set (run separately). | core-ts gate |

---

## 6. Gates & sequencing (impl phase, in order)

1. **COMMIT** the implementation before gates (branch `wicked/89719b7c…`).
2. `cargo test --lib` (core crate) — T1–T7.
3. `cargo clippy` (workspace, `-D warnings`).
4. `cargo fmt --check`.
5. **Public types changed** (new `CoreEvent::SandboxUnenforced`, new `os_sandbox` bool) ⇒ `cd crates/wicked-core-ts && cargo test` (T8) — separate, workspace-excluded.
6. PR merge protocol: branch, wait for bot reviewers + CI, address comments, merge.

---

## 7. Open questions carried (bound the edges, do NOT block this build)

- **OQ-SANDBOX-NET-001** — worker network tightening (008b). This build keeps network OPEN.
- **OQ-SANDBOX-MACOS-001** — `sandbox-exec` deprecated-but-present; ships on it (validator already depends on it in prod); contingency filed.
- **OQ-SANDBOX-WIN-001** — Windows write-containment floor; until then Windows = `BestEffort`, and now **observably disclosed** via `SandboxUnenforced` (A1) on any run that requested the floor.

---

## 8. Non-goals (must appear in shipped comments)

- NOT exfiltration/DLP; model-API egress + non-curated reads stay open.
- NOT Boundary 2 (`wicked-tools` governed MCP audit server) — separate later build.
- NOT the network-deny variant (008b).
- NOT a per-call audit trail — a sandbox is a floor, not policy; `Sandboxed` ≠ "governed/audited".

---

## 9. External-transform convention

No third-party library or service transforms a payload. Boundary 1 wraps wicked's own worker child in a kernel sandbox (macOS `sandbox-exec` SBPL / Linux `bwrap` namespaces); the disclosure event is wicked-owned. Consistent with DES-INPUT-GOV-008 §11 and the clarify deliverable.

ASSUMPTION[external-transform] library=none transform=none confidence=known :: Boundary 1 wraps wicked's own worker child in a kernel sandbox (sandbox-exec SBPL / bwrap namespaces) and emits a wicked-owned SandboxUnenforced disclosure; no third-party service normalizes, enriches, or converts any payload, so no external-transform entry applies.
