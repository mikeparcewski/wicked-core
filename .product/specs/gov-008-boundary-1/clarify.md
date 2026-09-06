# CLARIFY — GOV-008 Boundary 1: CLI-agnostic OS-sandbox DENY floor around the CLI worker spawn

**Umbrella:** wicked-core #360. **Source design:** `.product/DES-INPUT-GOV-008-os-sandbox-and-governed-mcp.md` §3 (+ §2.1 layering, §6/§8 rollout, §10 concrete artifacts).
**Phase:** CLARIFY. This document is analysis + design + plan ONLY. **No production code is written or committed in this phase** — implementation is a later phase. Every "will" below is a proposal for that phase, not a change made now.

**One-line intent:** promote the *already-working* validator-script OS sandbox (`src/validator.rs`) into a worker-spawn-capable module so the run **worktree + estate graph dir + in-boundary scratch** is the only kernel-writable root for **every** CLI seat, on both the wrapped and ACP spawn paths, LAYERED under (never replacing) the existing per-carrier governance.

**SCOPE — read this twice.** This is a **WRITE-containment** floor. It is **NOT exfiltration protection and NOT a read jail** (DES-INPUT-GOV-008 §2, §3.3–3.4). The model-API network channel and the non-curated read surface stay open by necessity; anything the agent legitimately reads can still leave over ordinary model traffic. Every comment the impl phase writes must say so — do not let `SandboxLevel::Sandboxed` be misread as "governed" or "DLP".

---

## 1. Source anchoring — the functions §3 names (verified at source)

All already exist in `src/validator.rs`, today wrapping validator *script* runs, not workers:

| Function / item | Loc | Role today | Reused how (Boundary 1) |
|---|---|---|---|
| `SandboxLevel` enum | `validator.rs:336` | honest disclosure: `Sandboxed` / `NetworkOnly` / `BestEffort` | **reused unchanged** on the worker wire |
| `sandbox_availability()` | `validator.rs:381` | probe PATH: `sandbox-exec`→`bwrap`→`firejail` | reused for capability disclosure |
| `find_on_path(bin)` | `validator.rs:399` | cross-platform PATH probe, returns abs `PathBuf` | reused — **abs path matters** (§4.4) |
| `secret_read_block_dirs()` | `validator.rs:428` | curated `~/.aws ~/.ssh ~/.gnupg ~/.config/wicked-council ~/.claude ~/.config/gh` | reused unchanged |
| `sbpl_quote(p)` | `validator.rs:459` | SBPL string escaping | reused unchanged |
| `macos_sandbox_profile(cwd, extra_write)` | `validator.rs:471` | build SBPL: allow default, deny net, deny secret reads, deny write* then allow write subpath(s) | **generalized** — multi write-root + network-policy param (§4.2, §4.3) |
| `detect_sandbox_launcher(cwd, extra_write)` | `validator.rs:516` | resolve wrapper argv + level; macOS → bwrap → firejail | **generalized** to the same signature shape (§4.2) |
| `SandboxLauncher { wrapper: Vec<String>, level }` | `validator.rs:423` | prependable wrapper argv | reused — this is the exact join point (§4.4) |

**Not reused for the worker:** `apply_minimal_env` / `ENV_PASSTHROUGH` (`validator.rs:604`), `run_bounded_status` / `kill_child_tree` / `reap_bounded` / `VALIDATOR_TIMEOUT`. Rationale in §4.5 — the worker keeps its own env discipline (`.hardened()` + deliberate gate-hook/API-key env) and its own lifecycle (`run_bounded` in execute_wrapped; cached ACP session). We lift the **profile + launcher-argv** construction, not the env-clear floor or the 120s script bound.

---

## 2. What "generalize" means concretely — two deltas vs. the validator use

Per DES-INPUT-GOV-008 §3.2, exactly two things change when the same machinery wraps a worker instead of a script:

1. **Write root = worktree (+ estate graph dir + in-boundary scratch), not the validator run dir.** The wrapped path already computes this exact set:
   - `armed_write_roots(cwd, extras, graph_dir)` (`execute_wrapped.rs:1027`) → the `WICKED_WRITE_ROOTS` value (`WRITE_ROOTS_ENV`, `gate_hook.rs:114`): unit `cwd` FIRST, launcher-declared `extra_write_roots`, plus estate-home `graph_dir` when applicable.
   - The sandbox profile's writable subpaths **must be the same set** so a policy-*allowed* write (worktree edit; estate graph `-wal`/`-shm`) is never kernel-killed. `macos_sandbox_profile`'s existing `extra_write` param (added for the coverage store, core#217) is the exact shape — generalized from one extra dir to the N-element write-root list.
   - In-boundary scratch `<cwd>/tmp` (minted by `redirect_scratch_into_boundary` / ACP's `scratch_tmp`) lives *under* `cwd`, so it is already covered by the worktree subpath — no separate carve-out needed, but the impl phase must confirm the canonicalized `cwd` subpath actually contains it (it does; `tmp` is a child of `cwd`).

2. **Network deny becomes a policy CHOICE, not unconditional.** The validator flatly denies network (`(deny network*)` / bwrap `--unshare-net`). **A CLI worker must reach its model endpoint or the run cannot function.** Per §3.2 + §8 Phase D, the network dimension is **deferred to 008b (OQ-SANDBOX-NET-001)**. So for THIS build: **the generalized profile does NOT deny network** — it emits the file-containment rules only. This is the single most important behavioural divergence from the validator profile and MUST be explicit in the profile-builder signature (a `NetworkPolicy::Allow` for workers vs. `NetworkPolicy::Deny` for validator scripts), so validator callers keep their deny and worker callers get allow — no accidental model-call breakage, no accidental silent widening for scripts. `SandboxLevel` already distinguishes the network dimension; filesystem containment is independent of the network choice.

Two further §3.2 notes that require **no code change**, only correct wiring:
- **Long-lived process, not a 120s script:** the wall-clock bound comes from the unit budget (existing `run_bounded` timeout / ACP session lifetime), NOT `VALIDATOR_TIMEOUT`. We do not lift `run_bounded_status`.
- **ACP session caching:** the sandbox wraps the **spawn**; a cached session (`probe_cached_session`, keyed `(run_id, cli_key)`) inherits the sandbox of its first spawn — identical lifetime/discipline to the `acp_governance_env` injection. Wrap unconditionally at spawn; never try to re-arm per turn.

---

## 3. Layering — what this adds, what it must NOT touch (DES-INPUT-GOV-008 §2.1)

Boundary 1 is a floor UNDER everything. The impl phase **removes nothing that works**:

- KEEP claude's `PreToolUse` gate-hook (`gate_hook.rs`, the `gov_env` wiring in `execute_wrapped.rs:1938+`). Boundary 1 sits beneath it.
- KEEP opencode's provisioned ACP `session/request_permission` (`acp_input_governance` + `acp_governance_env`, DES-INPUT-GOV-006).
- KEEP the wrapped-path `--disallowedTools` + `permissions.deny` fence.

Boundary 1 does not consult or replace any of these; it is a second, kernel-enforced containment layer under whatever verdict those carriers produce. A claude unit keeps its PreToolUse trail **and** gains the kernel write-jail; an opencode unit keeps its ACP gate **and** gains the same jail.

---

## 4. Design — the generalized module and its two wrap points

### 4.1 Module shape (recon proposal, §10)

Extract the reusable pieces into a worker-spawn-capable surface (either a new `src/worker_sandbox.rs` or a `pub(crate)` promotion of the validator internals — impl phase picks; the smaller-diff option is promoting `detect_sandbox_launcher` / `macos_sandbox_profile` / `SandboxLauncher` / `secret_read_block_dirs` / `sbpl_quote` to `pub(crate)` and adding the two generalized entry points below). The public surface the two spawn paths consume:

```
// PROPOSED (not built this phase)
enum NetworkPolicy { Deny, Allow }          // Deny = validator scripts; Allow = CLI workers (model egress stays open)

struct WorkerSandbox { wrapper: Vec<String>, level: SandboxLevel }

// write_roots[0] is the primary (cwd/worktree); the rest are estate graph dir etc.
fn detect_worker_sandbox(write_roots: &[PathBuf], net: NetworkPolicy) -> WorkerSandbox
```

`macos_sandbox_profile` is generalized to take `write_roots: &[Path]` (allow `file-write*` subpath for each canonicalizable root) + `net: NetworkPolicy` (emit `(deny network*)` only under `Deny`). The bwrap branch generalizes identically: `--bind` each write-root (bound LAST so it wins over tmpfs masks), and `--unshare-net` only under `Deny`. The existing single-`extra_write` validator callers are preserved by having the validator call the generalized builder with `[run_dir, coverage_dir]` + `NetworkPolicy::Deny` — byte-identical output to today.

### 4.2 Write-root source of truth

The worker write-root list is **derived from the same inputs `armed_write_roots` already uses** — do NOT re-derive independently (two-definitions-one-truth bug). Feed the sandbox the same `(cwd, extra_write_roots, graph_dir)` the wrapped path arms into `WICKED_WRITE_ROOTS`. This guarantees the kernel writable set == the gate-hook's advertised writable set, so no allowed write is ever kernel-denied and no denied write is ever kernel-allowed by mismatch.

### 4.3 macOS symlink canonicalization — carried forward

`macos_sandbox_profile` already canonicalizes (`/var → /private/var`). Each write-root must be canonicalized before it becomes an SBPL `subpath`. A root that fails to canonicalize is **dropped from the writable set** (fail-closed narrow), and if the *primary* (worktree) root fails to canonicalize the sandbox **cannot arm correctly** → fail-closed per §5.

### 4.4 The two wrap points — where the wrapper argv is prepended

Both paths today build `Command::new(argv[0]).args(argv[1..])`. Wrapping = prepend `WorkerSandbox.wrapper` (which ends in `sandbox-exec -p <profile>` or `bwrap … --`) so the child is `Command::new(<wrapper-abs-path>).args([wrapper-tail…, real_binary, real_args…])`.

- **Wrapped path:** `execute_wrapped.rs` ~L1937 (`let mut cmd = Command::new(&argv[0])`), the command later spawned by `run_bounded` (`cmd.spawn()`, L1942). The wrapper is prepended before `.hardened()`/env/`current_dir` are applied — env set on `cmd` is inherited by the real binary through the wrapper (both `sandbox-exec` and `bwrap` pass env through). `current_dir(cwd)` stays; bwrap also `--chdir`s.
- **ACP path:** `acp_runner.rs` `start_acp_process` `build_cmd(binary)` closure (L1408) — replace `Command::new(binary)` with the wrapped form. The `.hardened()`, `worker_config_dir`, `acp_governance_env`, `scratch_tmp`, `process_group(0)`, and the Windows `.cmd` retry (L1487) all still apply: the `.cmd` retry re-runs `build_cmd`, so it must wrap the *real* binary name, not the wrapper. Pipes (`stdin/stdout/stderr` piped) attach to the wrapper and flow through to the exec'd child unchanged.

**Absolute wrapper path is required.** `find_on_path` returns an absolute `PathBuf`; use it. Because `.hardened()` resets the child env (incl. possibly `PATH`), invoking the wrapper by bare name could fail to resolve. Prepend the absolute path.

**Process-group / die-with-parent interaction:** bwrap's `--die-with-parent --unshare-pid` ties the tree to the launcher; the ACP path additionally sets `process_group(0)` on the wrapper — the wrapper becomes group leader, existing pid-targeted kill/liveness (core#343) still reaches it. No conflict, but the impl phase must verify the ACP reaper still reaps the *wrapper* pid (it will — `Child` tracks the wrapper, which is the process ppid-linked to us).

### 4.5 Why the env floor is NOT lifted

`apply_minimal_env` clears the environment to an allowlist so an untrusted *script* cannot read API keys. A CLI worker **must** keep its model API key and the gate-hook env (`WICKED_DECISIONS_PATH`, `WICKED_WRITE_ROOTS`, scope/phase). The worker paths already do deliberate env discipline via `.hardened()` (FINDING-067: strips `WICKED_ESTATE_DB` etc., then sets exactly what's intended). Lifting `apply_minimal_env` would break every worker. Boundary 1 is filesystem (+ deferred network) containment only; env hygiene stays owned by the existing spawn code.

---

## 5. Fail-closed semantics (explicit requirement)

> If the sandbox cannot arm **where it is supported**, disclose unsandboxed or fail — never silently run as if contained.

Concrete rules for the impl phase:

- **Tool present + profile builds → arm.** `SandboxLevel::Sandboxed`, wrap, proceed.
- **Tool present but profile CANNOT build** (primary worktree root fails to canonicalize; `join_paths`-class failure): this is a supported platform where arming was expected and failed. **Fail-closed** — either refuse the spawn with a clear error, OR proceed unsandboxed but emit a loud disclosure event (mirroring `GovernanceUnenforced`, `event.rs:533`) tagged as a write-containment gap. Default to **disclose-and-degrade** to match the existing `GovernanceUnenforced` precedent (never a quiet gap, ORCHESTRATOR.md §10); the flag (§6) may be set to hard-fail for high-assurance deployments. **Never** proceed as `Sandboxed` when the wrapper did not actually arm.
- **No tool on PATH (`BestEffort`, notably all of Windows):** disclosed loudly ungoverned-on-write-containment, not silent. Windows = `BestEffort` always (`sandbox_availability` returns `(BestEffort, None)`), OQ-SANDBOX-WIN-001 tracks a future Job-Objects floor.
- **`firejail` only (`NetworkOnly`):** for a *worker* firejail buys **nothing** (it only denies network, which we explicitly want OPEN for workers). So on the worker path, `NetworkOnly`/firejail is equivalent to `BestEffort` for write-containment and MUST be disclosed as write-uncontained — never dressed as `Sandboxed`. (This differs from the validator use, where firejail's net-deny is the point.)

Disclosure event: reuse `SandboxLevel` on the wire. The impl phase adds a worker-spawn disclosure carrying `{ cli, level, tool }` — a sibling of `GovernanceUnenforced`, fired whenever a worker spawns at less than `Sandboxed` on a platform (or when arming was expected but degraded). core-ts caveat (§8) applies if this event is added to the wire enum.

---

## 6. Rollout — default-OFF flag (DES-INPUT-GOV-008 §8 Phase A, §6 fail-safe defaults)

Ship behind a **default-OFF** capability so Boundary 1 is opt-in per the doc's rollout discipline, mirroring the `acp_input_governance` default-`false` precedent (`types.rs:94`):

- Proposed config: a `#[serde(default)]` **plain `bool`** `os_sandbox` (working name; impl phase may align to `mcp_input_governance`'s sibling naming) on the seat/`AcpConfig` (or a global governance-context flag). **Default `false`** = current behaviour (no worker wrapping). `true` = arm Boundary 1 for that seat/run.
- **Inherit-on-omit merge semantics** identical to the `acp_governance_env` / DES-002 §3.6 precedent.
- Because it defaults off, the existing test suite and all current runs are byte-unchanged until a config explicitly opts in — the safe rollout the doc asks for.
- **core-ts caveat (CLAUDE.md, §10):** a new `AcpConfig`/`AgenticCli` field is deserialized by `crates/wicked-core-ts`. Use a plain `bool` with `#[serde(default)]`, **never a bare `Option`** (a `null` vs. absent wire-shape trap — TS `=== undefined` guards are dead code). If a public type gains this field, run `cd crates/wicked-core-ts && cargo test` separately (workspace-excluded, §Gates).

---

## 7. Test plan (the acceptance the impl phase must satisfy)

The task's four required proofs, made concrete. Tests are `--lib` unit/integration tests in the core crate; the kernel-denial tests are `#[cfg(unix)]`-gated and **skip cleanly when no real sandbox tool is on PATH** (assert-skip, so CI without `bwrap`/`sandbox-exec` is green but honest).

1. **Write OUTSIDE the worktree is KERNEL-DENIED — not merely "sandbox-exec was invoked".** Arm the worker sandbox over a temp worktree; run a child that attempts `echo x > <sibling-dir-outside-worktree>/pwned`; assert the write **did not happen** (file absent) AND the child saw an OS-level permission error (EPERM/`Operation not permitted`). Proving the *kernel* denied it, not that we merely constructed a wrapper argv. (Anti-cheat: a test that only asserts the wrapper argv contains `sandbox-exec` is explicitly INSUFFICIENT and must not be the sole coverage.)
2. **Write INSIDE the worktree succeeds.** Same arming; child writes `<worktree>/ok` and the estate-graph-dir extra root; assert both land. Guards against an over-tight profile killing legitimate governed writes (the core#217 failure mode).
3. **Wrap covers BOTH paths.** One test drives the wrapped spawn (`execute_wrapped` `run_bounded` surface), one drives `start_acp_process` — asserting each produces a wrapped `Command` (wrapper abs-path prepended, real binary + args preserved, `.cmd` retry still wraps the real name, `current_dir`/env/pipes intact). Combined with (1) end-to-end on at least one path to prove the wrap is load-bearing, not cosmetic.
4. **Fail-closed fires when the launcher is absent / arming degraded.** With no sandbox tool on PATH (or a forced canonicalization failure), assert the spawn **discloses** (`SandboxLevel::BestEffort`/degrade event emitted) and does **not** silently claim `Sandboxed`. Assert the network-policy divergence too: a worker profile does **not** contain `(deny network*)` (model egress preserved), while the validator profile still does.

Plus a **regression guard:** validator script runs still emit a byte-identical profile (net-deny + single run-dir) after the generalization — the shared builder called with `NetworkPolicy::Deny` + `[run_dir, coverage_dir]` matches the pre-refactor golden.

---

## 8. Gates (impl phase, in order — COMMIT before gates)

Per the task and ecosystem `CLAUDE.md`:

1. **Commit** the implementation before running gates (branch already `wicked/89719b7c…`).
2. `cargo test --lib` (core crate).
3. `cargo clippy` (workspace, `-D warnings` per repo norm).
4. `cargo fmt --check`.
5. **If any public type changed** (the new `os_sandbox` bool on `AcpConfig`/`AgenticCli`, or a new wire event): `cd crates/wicked-core-ts && cargo test` — it is **workspace-excluded on purpose** (its `[profile.release]` shapes the shipped `.node`), so `cargo test --workspace` never touches it. Must be run separately.

Then the PR merge protocol (branch, wait for bot reviewers + CI, address comments, merge).

---

## 9. Open questions carried (do NOT block this build; they bound its edges — §7 of the design)

- **OQ-SANDBOX-NET-001** — daemon-side model proxy (deny-all-net) vs. allow-model-host-only. Blocks ONLY the network-tightening (008b/Phase D), not this filesystem-containment build. This build keeps network **open**.
- **OQ-SANDBOX-MACOS-001** — `sandbox-exec` is deprecated-but-present. This build ships on it (validator already depends on it in production). Contingency (live functionality probe on the pinned macOS target; App Sandbox vs. Endpoint Security assessment) is filed, not resolved here.
- **OQ-SANDBOX-WIN-001** — Windows write-containment floor (Job Objects / restricted tokens / WSL2). Until then Windows is honestly `BestEffort`.

## 10. Non-goals for this build (say so in comments — §2/§3.3–3.4)

- NOT exfiltration/DLP. Model-API egress stays open; reads outside the curated denylist stay open. Boundary 1 denies *writes* outside the worktree + blocks reads of the curated secret dirs, nothing more.
- NOT Boundary 2 (the `wicked-tools` governed MCP audit server). Separate, later build.
- NOT the network-deny variant (008b). This build leaves worker network open.
- NOT a policy/audit trail. A sandbox is a floor, not a per-call record — that is Boundary 2's job. Reuse the `SandboxLevel` honest disclosure; never let `Sandboxed` be read as "governed/audited".

---

## 11. External-transform convention

No third-party library or service transforms a payload in this build. The OS sandbox is a kernel-enforced policy (macOS `sandbox-exec` SBPL / Linux `bwrap` namespaces) applied to wicked's own child process — not a payload transformation. Consistent with DES-INPUT-GOV-008 §11.

ASSUMPTION[external-transform] library=none transform=none confidence=known :: Boundary 1 wraps wicked's own worker child in a kernel sandbox (sandbox-exec SBPL / bwrap namespaces); no third-party service normalizes, enriches, or converts any payload, so no external-transform entry applies.
