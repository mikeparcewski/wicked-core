---
id: ops-watch
title: Risky shell calls the Watchtower flags (never blocks)
status: active
date: 2026-10-02
enforcement_class: policy
steering_type: operations
applies_to: [adversarial-review, analyze, architecture, build, clarify, cleanup, coverage, critique, cutover, design, domain-graph, domain_coverage, execute, extract, fix, implement, plan, produce, reproduce, review, security_review, survey, test, test_plan, triage, understand, verify, walkthrough_plan]
scope: wiki:governance
domain: ops-watch
confidence: 1.0
---

# Risky shell calls the Watchtower flags

A governed worker's shell call can be legitimate and still worth a look: a recursive force delete
outside a scratch dir, a force push, killing processes by name, a `curl | sh`, a publish or a
merge from inside a run. None of these should stop the run. The operator should see them.

Every rule here is `effect: warn`. The engine's input hook evaluates them per tool call. A
triggered rule is recorded on the decision without blocking: the decision stays `allow` (a doc-lane
warn rule carries no obligations), and the rule id is in the claim's `policy_ids`. The gate fold
replays each decision as `governanceHookFired`, whose `firedPolicies` carries the ids. The
Watchtower's `risky-call` entry (DES-trigger-registry §4.4 row 7) flags each fired warn rule,
naming it. Coverage is governed
units only: a unit that ran ungoverned emits `governanceUnenforced`, and the Watchtower shows
"not checked" for it.

Triggers are regexes over the canonical JSON of the evaluated tool call, anchored on its
`"command"` field AND on a shell tool name (`Bash`, `shell`, `execute`, …), so only a shell command
can fire them: a `Write` whose content mentions `sudo` does not, and neither does an `Edit` whose
arguments happen to carry a `command` field. The command is a JSON string there, so a newline reads as the two characters `\n`,
which is why line-start matches accept `\\n` as well as a word boundary. `--force` is narrowed to the commands where it is
destructive (`git push`, `git worktree remove`), as `GOV-FORCE-PUSH` does in the evals.

Measured against the labelled set of 217 governed tool calls (`laya-eval` tool-call set, 5
risky): precision 0.833, recall 1.000, F1 0.909 (floor 0.77). One false positive: a `rm -rf` of a
`__pycache__` dir.

## Rules

- `OPS-WATCH-001` (warn): A recursive force delete outside a scratch or build directory deserves a
  look: `rm -rf` of a path that does not start with `tmp`, `target`, `_` or a quoted variable
  (`tar`, `tarball` and `trash` do fire).
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:(\\n|\b)rm\s+-(rf|fr)\s+([^<_\s\\/t]|t([^am]|a([^r]|r([^g]|g([^e]|e[^t]))))|tm[^p]|<[^t]|/[^t]|/t[^m]|/tm[^p]))|"command":"(?:[^"\\]|\\.)*?(?:(\\n|\b)rm\s+-(rf|fr)\s+([^<_\s\\/t]|t([^am]|a([^r]|r([^g]|g([^e]|e[^t]))))|tm[^p]|<[^t]|/[^t]|/t[^m]|/tm[^p])).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-002` (warn): A force push rewrites a remote branch's history.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bgit\s+push\b[^;&|\\]*\s(-f|--force)\b)|"command":"(?:[^"\\]|\\.)*?(?:\bgit\s+push\b[^;&|\\]*\s(-f|--force)\b).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-003` (warn): Force-removing a git worktree discards whatever was uncommitted in it.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bgit\s+worktree\s+remove\b[^;&|\\]*\s(-f|--force)\b)|"command":"(?:[^"\\]|\\.)*?(?:\bgit\s+worktree\s+remove\b[^;&|\\]*\s(-f|--force)\b).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-004` (warn): Killing processes by name, or sending SIGKILL, can reach processes the
  run does not own.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:(\\n|\b)(pkill|killall)\s|(\\n|\b)kill\s+(-9|-KILL|-SIGKILL|-s\s+(KILL|SIGKILL)|-s\s+9)\b)|"command":"(?:[^"\\]|\\.)*?(?:(\\n|\b)(pkill|killall)\s|(\\n|\b)kill\s+(-9|-KILL|-SIGKILL|-s\s+(KILL|SIGKILL)|-s\s+9)\b).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-005` (warn): `sudo` inside a governed run escalates past the run's boundary.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bsudo\s)|"command":"(?:[^"\\]|\\.)*?(?:\bsudo\s).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-006` (warn): `playwright install --with-deps` installs system packages: it changes
  the host, not the repository.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bplaywright\s+install\b[^;&|\\]*\s--with-deps\b)|"command":"(?:[^"\\]|\\.)*?(?:\bplaywright\s+install\b[^;&|\\]*\s--with-deps\b).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-007` (warn): `npx --yes` downloads and runs a package without a prompt.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bnpx\s+(--yes|-y)\s)|"command":"(?:[^"\\]|\\.)*?(?:\bnpx\s+(--yes|-y)\s).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-008` (warn): Piping a download into a shell runs code no one reviewed.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bcurl\b[^|;&\\]*\|\s*(ba|z)?sh\b)|"command":"(?:[^"\\]|\\.)*?(?:\bcurl\b[^|;&\\]*\|\s*(ba|z)?sh\b).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-009` (warn): Publishing a package or cutting a release from inside a run leaves the
  machine.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bnpm\s+publish\b|\bgh\s+release\s+create\b)|"command":"(?:[^"\\]|\\.)*?(?:\bnpm\s+publish\b|\bgh\s+release\s+create\b).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-010` (warn): Merging a pull request from inside a run bypasses the operator's merge
  gate.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bgh\s+pr\s+merge\b)|"command":"(?:[^"\\]|\\.)*?(?:\bgh\s+pr\s+merge\b).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")
- `OPS-WATCH-011` (warn): A hard reset or a forced branch delete throws away commits.
  effect: warn
  trigger: (?:"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)".*?"command":"(?:[^"\\]|\\.)*?(?:\bgit\s+reset\s+--hard\b|\bgit\s+branch\s+-D\s)|"command":"(?:[^"\\]|\\.)*?(?:\bgit\s+reset\s+--hard\b|\bgit\s+branch\s+-D\s).*?"tool":"(?:Bash|bash|shell|Shell|execute|exec|exec_command|run_shell_command|run_terminal_cmd|terminal)")

## Sources

- DES-trigger-registry §4.4 row 7 and slice W2.
- The starter denylist of the laya evaluation (`RX_RISK`), rewritten without look-around (the
  engine's regex dialect has none).
