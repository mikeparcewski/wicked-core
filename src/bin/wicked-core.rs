//! Thin operator CLI over the COE library — the replacement entry point for the retired
//! `wicked-agent` binary. All composition lives in `wicked_core`; this is just argv + printing.
//!
//!   wicked-core status                            # list sessions + units on the store
//!   wicked-core repos                             # list registered repositories
//!   wicked-core register-repo --path <dir> [--name N]   # register a git repo to run within
//!   wicked-core run --problem "Do X. Do Y" \      # interactive governed run (streams events)
//!       [--repo <id>] [--confirm none|all|before:N] [--session <id>] [--clis <csv>]
//!   wicked-core resume --session <id>             # resume a paused/interrupted run
//!   wicked-core cancel --session <id>             # cancel a run
//!   wicked-core launch --problem "..."            # STUB self-test: deterministic stub output, no real CLI, no gates
//!   wicked-core gate-hook --scope S --phase P     # PreToolUse governance hook (claude invokes this)
//!   wicked-core output-gate-hook --scope S --phase P  # per-OUTPUT guardrail: governs generated
//!       # output text on stdin (policy-over-output + conformance-rule recall) → decisions.ndjson
//!   wicked-core provision-validator --criterion "..."   # author a deterministic validator (UNAPPROVED)
//!   wicked-core approve-validator --pin <pin>     # approve a vaulted validator → the pin to put in a def
//!   wicked-core gate-phase --workflow <base-id> --phase <phase-id> --criterion "..." [--out <dir>]
//!       # author+approve a validator for the criterion, PIN it onto that phase, and write a gated
//!       # drop-in workflow (new id) — the one path that turns a shipped, ungated workflow INTO a
//!       # gated one so the rev0.4 dual-validator gate actually engages
//!   wicked-core seed-domain-validators           # seed the deterministic coverage validator the
//!       # shipped domain-extraction.json gate pins, so that drop-in runs instead of failing closed
//!   wicked-core rules ingest <dir>               # populate governance policies (deny) + conformance
//!       # rules (recall→obligation) into the store: <dir>/policies/*.json + <dir>/rules/*.json +
//!       # frontmattered markdown rule docs anywhere under <dir> (AW-3 MarkdownAdapter); doc
//!       # frontmatter `domain:` groups its rules under a native RuleSet node with Contains
//!       # membership (AW-13 — what RulesInventory lists)
//!   wicked-core rules fanout <dir> \              # fan ONE ruleset out across the deliberate store
//!       (--enforcement-db F | --enforcement-crew-api URL) \      # split (AW-5): enforcement copy
//!       --discovery-db F [--discovery-db F]... \  # + discovery graph copies (one per live repo
//!       --knowledge-db F [--knowledge-db F]... \  #   under --scope workspace) + knowledge rationale
//!       [--scope repo|workspace] [--knowledge-scope S] [--manifest OUT.json]
//!       # Every cli lane is smoke-verified against the SAME db a worker is handed; the manifest
//!       # (keyed on PAT-/POL- ids) is the receipt. A daemon-held store is NEVER CLI-written:
//!       # --enforcement-crew-api records the pending transport + emits the POST payload instead.
//!   wicked-core rules relink [--ambiguity-cap N] [--json]   # re-derive rule→code Governs edges from
//!       # qualified symbol_refs at the current epoch (AW-9) — run after every `wicked-estate index`;
//!       # unresolvable refs are REPORTED as drift, never dropped. With --knowledge/--xedge (or
//!       # $WICKED_KNOWLEDGE_DB/$WICKED_XEDGE_DB) also writes knowledge→code about-xedges
//!       # (the knowledge.relate_code seam) for docs ingested into the knowledge domain
//!   wicked-core rules eval [--corpus <evals:scope | dir>] [--type <t>] [--db <F>] \
//!       [--knowledge-db <F>] [--json]              # replay a behavior corpus through the REAL
//!       # SELECT→DECIDE gate path and score caught/gap/false-positive per sample; gaps carry
//!       # nearest non-firing rules by embedding similarity (facet-only keyword hints, marked,
//!       # when no usable embeddings exist). Read-only on the rules store.
//!   wicked-core rules eval --import <name> [<dir>] [--knowledge-db <F>]   # ingest a corpus into
//!       # the estate knowledge store under evals:<name> (id-keyed, WITH embeddings) and print
//!       # the {imported, scope, embedded} receipt
//!   wicked-core rules drift [--dir <docs>] [--json]  # report the residue re-ingest can't self-heal
//!       # (AW-10): orphaned / uningested / unresolvable / unlinked / extraneous; read-only; exits 3
//!       # when residue is found (0 clean, 1 operational error) — run with the same --dir as ingest
//!   wicked-core rules recall [--language L] [--layer L] [--framework F] \
//!       [--severity info|warn|error|critical] [--rule-type pattern|policy] \
//!       [--type <steering-type>] [--json]
//!       # the AW-17 recall-REPORT: the conformance rules that APPLY to the query facets,
//!       # severity-ordered (critical→info, then weight desc, then id), each citing rule id +
//!       # provenance ref (the wiki URI a CI comment links to). --type is the STEERING facet
//!       # (architecture|development|security|testing|operations|compliance|design-ux).
//!       # Read-only, and strictly a report: exit 0 even when rules match
//!       # (v1 never blocks — arch-R15); 1 = operational error only
//!   wicked-core rules list [--type <t>] [--include-retired] [--language L] [--layer L] \
//!       [--framework F] [--severity S] [--rule-type T] [--json]
//!       # the STEERING management LISTING over the unified store: recall-only AND effect-bearing
//!       # (decide-lane / migrated-policy) rules, with retired rows included under
//!       # --include-retired (recall never returns them — this is the audit view). Read-only.
//!   wicked-core coverage [--out F]                # recompute front-half coverage FROM THE STORE →
//!       # coverage-report.json (schema-exact; two-predicate: bare/description-only behavior nodes are holes)
//!   wicked-core domain-graph [--coverage F] [--out F]  # translate the annotated estate graph into
//!       # requirements_graph.json (front-half coverage RECOMPUTED from the store, FAIL-CLOSED on < 1.0;
//!       # a supplied --coverage file is an optional cross-check that must agree; modern package-dir grouping)
//!   [--db <path>]                                 # else $WICKED_ESTATE_DB, else ./wicked-estate.db

use std::io::BufRead;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wicked_core::{
    registry_roster, run_gate_hook, run_output_gate_hook, Core, CoreEvent, EntityMode,
    HumanConfirm, HumanDecision, LaunchSpec, RepoSpec, SessionStatus, WorkflowRegistry,
    WrappedCliStepRunner, COVERAGE_DB_ENV, ESTATE_DB_ENV, GATE_DB_ENV, GATE_PHASE_ENV,
    GATE_PHASE_ID_ENV, GATE_SCOPE_ENV,
};

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// A non-empty environment variable, or `None`.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Resolve a gate-hook argument from `--flag` (standalone invocations) ELSE `env_var` (the launcher sets
/// it there so it never rides the shell-executed hook command). Empty if neither is set.
fn resolve_hook_arg(args: &[String], flag_name: &str, env_var: &str) -> String {
    flag(args, flag_name)
        .or_else(|| env_nonempty(env_var))
        .unwrap_or_default()
}

/// The OPTIONAL workflow-phase alias for a hook run (`--phase-id` ELSE `WICKED_GATE_PHASE_ID`).
/// Unlike scope/phase this has no meaningful default: absent means "no alias", which must stay
/// distinct from the empty string (an empty token would match a policy authored as `applies_to: [""]`).
fn hook_phase_alias(args: &[String]) -> Option<String> {
    flag(args, "--phase-id")
        .filter(|s| !s.is_empty())
        .or_else(|| env_nonempty(GATE_PHASE_ID_ENV))
}

/// The authoritative subcommand list. Printed BOTH by `--help` (stdout, exit 0) and by the
/// unknown-subcommand arm (stderr, exit 2). One string: a second hand-written list beside it is how
/// a documented command set drifts from the real one.
const ROOT_USAGE: &str = "usage: wicked-core <status | repos | register-repo --path <dir> | \
     run --problem \"...\" [--repo <id>] [--confirm none|all|before:N] [--workflow <id>] [--clis <csv>] | \
     resume --session <id> | reattach --session <id> | cancel --session <id> | \
     launch --problem \"...\" [--workflow <id>] (STUB self-test — deterministic, no real CLI, no gates) | \
     provision-validator --criterion \"...\" | approve-validator --pin <pin> | \
     seed-domain-validators (seed the coverage validator for domain-extraction.json) | \
     gate-phase --workflow <base-id> --phase <phase-id> --criterion \"...\" [--out <dir>] \
     (author+approve+pin a validator onto a phase → a gated drop-in workflow)> [--db <path>]";

/// Per-subcommand usage, consulted by ONE `--help` chokepoint.
///
/// `--help` used to be handled (or not) inside each subcommand. `domain-graph` guarded it because it
/// writes a file; `seed-domain-validators` did not, so `--help` SEEDED AND APPROVED a validator, and
/// `coverage --help` ran a full coverage pass over the store. Asking a tool what it does should never
/// be the same as telling it to do it (core#132).
///
/// A table plus one guard, not a guard per subcommand: the per-subcommand form is what left most of
/// them unguarded, and a new subcommand added tomorrow inherits this by default instead of having to
/// remember. Same reason every spawn goes through one hardened helper.
const SUBCOMMAND_USAGE: &[(&str, &str)] = &[
    (
        "domain-graph",
        "wicked-core domain-graph [--db <path>] [--coverage <F>] [--out <F>] [--schema-version <V>]\n  \
         Translate the ANNOTATED estate graph into requirements_graph.json (default out: \
         .wicked-estate/requirements/requirements_graph.json, cwd-relative). Fails closed when the \
         domain-extraction front-half has not annotated the graph.",
    ),
    (
        "coverage",
        "wicked-core coverage [--db <path>] [--json]\n  \
         Recompute front-half coverage from the store and print the report. Reads only.",
    ),
    (
        "seed-domain-validators",
        "wicked-core seed-domain-validators [--db <path>]\n  \
         Vault and APPROVE the domain-extraction coverage validator. WRITES to the store.",
    ),
    (
        "provision-validator",
        "wicked-core provision-validator --criterion <TEXT> [--db <path>]\n  \
         Author a deterministic validator UNAPPROVED. Approval is a separate, audited step \
         (`approve-validator`) — authoring never authorizes running.",
    ),
    (
        "approve-validator",
        "wicked-core approve-validator --pin <PIN> [--db <path>]\n  \
         Approve a vaulted validator so a phase may pin it.",
    ),
    (
        "gate-phase",
        "wicked-core gate-phase --scope <S> --phase <P> [--db <path>]\n  \
         Evaluate the governance gate for one phase and print the decision.",
    ),
    (
        "output-gate-hook",
        "wicked-core output-gate-hook [--scope <S>] [--phase <P>] [--db <path>]\n  \
         The per-OUTPUT sibling of `gate-hook`: governs generated output text on stdin. Same \
         read-only-then-append discipline; exits with the gate's code (2 = deny).",
    ),
    (
        "gate-hook",
        "wicked-core gate-hook [--protocol-version]\n  \
         The PreToolUse hook. Arguments travel by environment variable; `--protocol-version` prints \
         the handshake the engine checks before arming a run.",
    ),
    (
        "rules",
        "wicked-core rules ingest <dir> [--db <F>]\n  \
         Populate ONE store with governance policies (deny) + conformance rules (recall→obligation): \
         <dir>/policies/*.json + <dir>/rules/*.json + frontmattered markdown docs. WRITES to the store.\n\
         wicked-core rules fanout <dir> (--enforcement-db <F> | --enforcement-crew-api <URL>) \
         --discovery-db <F>... --knowledge-db <F>... [--scope repo|workspace] [--knowledge-scope <S>] \
         [--manifest <OUT.json>]\n  \
         Fan ONE ruleset out across the deliberate store split (AW-5): enforcement copy + discovery \
         graph copies (one per live repo under --scope workspace) + knowledge rationale chunks, each \
         cli lane smoke-verified against the worker-visible db. A daemon-held store is NEVER \
         CLI-written: --enforcement-crew-api records the pending transport + emits the POST payload. \
         WRITES to every cli lane store.\n\
         wicked-core rules recall [--db <F>] [--language <L>] [--layer <L>] [--framework <F>] \
         [--severity info|warn|error|critical] [--rule-type pattern|policy] \
         [--type <steering-type>] [--json]\n  \
         The AW-17 recall-REPORT: the conformance rules that APPLY to the query facets, \
         severity-ordered (critical→info, then weight desc, then id), each citing rule id + \
         provenance ref (the wiki URI a CI comment links to). --type filters on the STEERING type \
         (architecture|development|security|testing|operations|compliance|design-ux). READ-ONLY, \
         and strictly a report: exit 0 even when rules match (v1 of \
         the CI conformance seam never blocks — arch-R15); 1 = operational error only.\n\
         wicked-core rules list [--db <F>] [--type <steering-type>] [--include-retired] \
         [--language <L>] [--layer <L>] [--framework <F>] [--severity <S>] [--rule-type <T>] \
         [--json]\n  \
         The STEERING management LISTING over the unified steering-rule store: recall-only AND \
         effect-bearing (decide-lane / migrated-policy) rules alike, with retired rows included \
         under --include-retired — the audit view recall deliberately never serves. READ-ONLY.\n\
         wicked-core rules scoreboard [--db <F>] [--dir <docs>] [--ambiguity-cap <N>] [--json]\n  \
         The AW-23 population/connection scoreboard: % statements typed into enforcement classes \
         (needs --dir — the class lives in doc frontmatter), % symbol_refs resolving at the current \
         epoch, denial claims citing wiki rules (evidenced_by edges / Governs evidence_count), and \
         recall volume (documented unavailable in-band — the store keeps no recall telemetry). \
         READ-ONLY, strictly a report: exit 0 = report produced, 1 = operational error.\n\
         wicked-core rules eval [--corpus <evals:scope | dir>] [--type <steering-type>] \
         [--db <rules.db>] [--knowledge-db <F>] [--json]\n  \
         Replay a behavior corpus (default: the built-in dev-behaviors corpus) through the REAL \
         SELECT→DECIDE gate path against the rules in --db: a bad sample a blocking rule fires for \
         is CAUGHT, one nothing fires for is a GAP (with nearest non-firing rules by embedding \
         similarity over the knowledge store's rule-rationale chunks — degrades HONESTLY to \
         facet-only keyword hints, marked on the report, when no usable embeddings exist), and a \
         good sample a blocking rule fires for is a FALSE POSITIVE. READ-ONLY on the rules store, \
         strictly a report: exit 0 = report produced (gaps included), 1 = operational error.\n\
         wicked-core rules eval --import <name> [<dir>] [--knowledge-db <F>]\n  \
         Ingest a corpus (a dir of sample *.json, or the built-in corpus when <dir> is omitted) \
         into the estate KNOWLEDGE store under scope evals:<name>, id-keyed WITH embeddings, and \
         print the {imported, scope, embedded} receipt. WRITES to the knowledge store only \
         (default ~/.wicked-estate/knowledge.db — always overridable via --knowledge-db).",
    ),
];

/// Top-level usage. The subcommand list is DERIVED from `SUBCOMMAND_USAGE`, so documenting a new
/// subcommand in one place is enough — a hand-maintained second list is how these drift.
fn print_root_usage() {
    println!("{ROOT_USAGE}");
    println!("\nDetailed help is available per subcommand:");
    for (name, _) in SUBCOMMAND_USAGE {
        println!("  wicked-core {name} --help");
    }
}

/// Print usage for `sub` if `args` asks for help. Returns true when it handled the call.
fn handled_help(sub: &str, args: &[String]) -> bool {
    if !args.iter().any(|a| a == "--help" || a == "-h") {
        return false;
    }
    match SUBCOMMAND_USAGE.iter().find(|(name, _)| *name == sub) {
        Some((_, usage)) => println!("{usage}"),
        // An undocumented subcommand still must not EXECUTE on `--help`.
        None => println!(
            "wicked-core {sub} — no usage recorded; see `wicked-core` for the command list"
        ),
    }
    true
}

fn store_path(args: &[String]) -> String {
    flag(args, "--db")
        // `WICKED_COVERAGE_DB` first: it is what a validator script is given, and it is the only
        // store such a script is entitled to. `WICKED_ESTATE_DB` remains for ordinary CLI use, where
        // the operator IS the one choosing the store (core#166).
        .or_else(|| {
            std::env::var(COVERAGE_DB_ENV)
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| std::env::var(ESTATE_DB_ENV).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "wicked-estate.db".to_string())
}

/// Parse `--confirm none|all|before:N` into a [`HumanConfirm`] policy (default `None` when absent).
/// Delegates to the ONE canonical parser so the CLI cannot disagree with the bus/napi/HTTP paths, and
/// FAILS CLOSED: a typo'd `--confirm` is an error, not a silent ungated run (FINDING-019).
fn parse_confirm(args: &[String]) -> Result<HumanConfirm, String> {
    HumanConfirm::parse(flag(args, "--confirm").as_deref())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // `--version` before ANY dispatch: an installer must be able to ask which build it just placed
    // without that answer depending on store state, policy, or a subcommand succeeding. The
    // installed CLI previously had no way to report this at all, so `install-local.py` could verify
    // the validator PIN but not the BUILD — half of FINDING-081's family, one level up.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("wicked-core {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    // gate-hook runs as a SUBPROCESS that claude spawns per tool-call. It must NOT spawn the actor
    // (it never writes the store — it only reads policies and appends decisions.ndjson), so handle
    // it before `Core::spawn` and exit with the gate's code (2 = deny ⇒ claude aborts the call).
    if args.get(1).map(String::as_str) == Some("gate-hook") {
        // `--help` must DOCUMENT, never execute: this hook opens a store and appends a decision.
        // It is dispatched ahead of the general chokepoint below (it must never spawn the actor),
        // so it carries the guard itself (core#132). Checked FIRST — a question about what a
        // command does has to be answerable without doing any of it.
        if handled_help("gate-hook", &args) {
            return;
        }
        // The handshake the launcher probes BEFORE arming a run. Answered before anything else so it
        // stays cheap and cannot be affected by store/policy state (core#167).
        if args.iter().any(|a| a == "--protocol-version") {
            println!("{}", wicked_core::protocol_version_line());
            std::process::exit(0);
        }
        // Resolve scope/phase/db from argv (standalone) ELSE the env the launcher sets. The injected
        // command carries NONE of these in the shell string (only the trusted exe) — scope/phase/db all
        // travel via env, so caller-controlled ids can't inject shell metacharacters (security fix).
        let scope = resolve_hook_arg(&args, "--scope", GATE_SCOPE_ENV);
        let phase = resolve_hook_arg(&args, "--phase", GATE_PHASE_ENV);
        let phase_id = hook_phase_alias(&args);
        let db = flag(&args, "--db").or_else(|| env_nonempty(GATE_DB_ENV));
        std::process::exit(run_gate_hook(
            &scope,
            &phase,
            phase_id.as_deref(),
            db.as_deref(),
        ));
    }

    // output-gate-hook is the PER-OUTPUT sibling: same read-only-then-append discipline, but it
    // governs the generated OUTPUT text (on stdin) instead of a proposed tool input. Also exits with
    // the gate's code (2 = deny) and must run before `Core::spawn`.
    if args.get(1).map(String::as_str) == Some("output-gate-hook") {
        // `--help` must DOCUMENT, never execute: this hook opens a store and appends a
        // decision. It is dispatched ahead of the general chokepoint below (it must not
        // spawn the actor), so it carries the guard itself (core#132).
        if handled_help("output-gate-hook", &args) {
            return;
        }
        let scope = resolve_hook_arg(&args, "--scope", GATE_SCOPE_ENV);
        let phase = resolve_hook_arg(&args, "--phase", GATE_PHASE_ENV);
        let phase_id = hook_phase_alias(&args);
        let db = flag(&args, "--db").or_else(|| env_nonempty(GATE_DB_ENV));
        std::process::exit(run_output_gate_hook(
            &scope,
            &phase,
            phase_id.as_deref(),
            db.as_deref(),
        ));
    }

    // provision-validator / approve-validator drive the rev0.4 pin+vault authoring flow DIRECTLY on the
    // store (author→approve→vault). Like gate-hook they must NOT spawn the actor — they open the store as
    // its SOLE writer for a brief command and exit — so handle them before `Core::spawn` (spawning the
    // actor too would put a second writer on the same SQLite file, breaking the single-writer invariant).
    // ONE `--help` chokepoint, before any subcommand runs. Asking what a command does must never be
    // the same as telling it to do it (core#132).
    //
    // `--help` with no subcommand is the ROOT request, not a subcommand named `--help`; routing it
    // through the table would answer "no usage recorded" for the one form users try first.
    match args.get(1).map(String::as_str) {
        // NOT `None`: bare `wicked-core` STARTS THE ENGINE (spawns the actor, serves the API).
        // Intercepting it here would turn the daemon's own launch command into a help screen.
        Some("--help") | Some("-h") | Some("help") => {
            print_root_usage();
            return;
        }
        Some(sub) if handled_help(sub, &args) => return,
        _ => {}
    }

    match args.get(1).map(String::as_str) {
        Some("provision-validator") => return provision_validator_cmd(&args),
        Some("approve-validator") => return approve_validator_cmd(&args),
        Some("gate-phase") => return gate_phase_cmd(&args),
        Some("seed-domain-validators") => return seed_domain_validators_cmd(&args),
        Some("domain-graph") => return domain_graph_cmd(&args),
        Some("coverage") => return coverage_cmd(&args),
        // `rules ingest <dir>` populates a run's store with governance policies + conformance rules so
        // the output guardrail has something to deny/recall against (core#26). Opens the store directly.
        Some("rules") if args.get(2).map(String::as_str) == Some("ingest") => {
            return rules_ingest_cmd(&args)
        }
        // `rules fanout <dir>` fans one ruleset out across the deliberate store split (AW-5):
        // enforcement + discovery + knowledge lanes, each smoke-verified; the manifest is the receipt.
        Some("rules") if args.get(2).map(String::as_str) == Some("fanout") => {
            return rules_fanout_cmd(&args)
        }
        // `rules relink` re-derives rule→code Governs edges from qualified symbol_refs (AW-9). Like
        // ingest it opens the store directly as a brief sole writer — never via the actor.
        Some("rules") if args.get(2).map(String::as_str) == Some("relink") => {
            return rules_relink_cmd(&args)
        }
        // `rules drift` reports what re-ingest can't self-heal (AW-10). Strictly read-only, so it
        // opens the store with open_store_ro and can run beside a live single-writer daemon.
        Some("rules") if args.get(2).map(String::as_str) == Some("drift") => {
            return rules_drift_cmd(&args)
        }
        // `rules recall` is the AW-17 recall-report: the applicable ruleset, severity-ordered, each
        // rule citing id + provenance ref. Read-only (open_store_ro), safe beside a live daemon.
        Some("rules") if args.get(2).map(String::as_str) == Some("recall") => {
            return rules_recall_cmd(&args)
        }
        // `rules list` is the STEERING management listing over the unified store — recall-only AND
        // decide-lane rules, retired rows on request. Read-only (open_store_ro), daemon-safe.
        Some("rules") if args.get(2).map(String::as_str) == Some("list") => {
            return rules_list_cmd(&args)
        }
        // `rules scoreboard` is the AW-23 population/connection scoreboard: typed %, resolving %,
        // denials citing wiki rules. Read-only (open_store_ro), safe beside a live daemon.
        Some("rules") if args.get(2).map(String::as_str) == Some("scoreboard") => {
            return rules_scoreboard_cmd(&args)
        }
        // `rules retire` is the AW-24 kill switch: manifest-keyed retirement propagated across
        // every fan-out lane in one op. Writes cli lane stores directly (brief sole writer, like
        // ingest/fanout); a daemon-held store is recorded PENDING, never written.
        Some("rules") if args.get(2).map(String::as_str) == Some("retire") => {
            return rules_retire_cmd(&args)
        }
        // `rules eval` replays a behavior corpus through the REAL SELECT→DECIDE gate path and
        // scores caught/gap/false-positive per sample. Read-only on the rules store
        // (open_store_ro, daemon-safe); `--import` is the one write path (knowledge store only).
        Some("rules") if args.get(2).map(String::as_str) == Some("eval") => {
            return rules_eval_cmd(&args)
        }
        _ => {}
    }

    let core = Core::spawn(store_path(&args));

    match args.get(1).map(String::as_str) {
        Some("status") => print_status(&core),
        Some("repos") => match core.list_repos() {
            Ok(rs) if rs.is_empty() => println!("(no repos registered)"),
            Ok(rs) => {
                for r in rs {
                    println!("{}  {}  [{}]", r.id, r.root_path, r.default_branch);
                }
            }
            Err(e) => fail(&format!("repos failed: {e}")),
        },
        Some("register-repo") => {
            let Some(path) = flag(&args, "--path") else {
                fail("register-repo requires --path <dir>");
                return;
            };
            let name = flag(&args, "--name").unwrap_or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "repo".to_string())
            });
            match core.register_repo(RepoSpec {
                name,
                root_path: path,
                registered_at: now_secs(),
            }) {
                Ok(e) => println!(
                    "registered {} → {} [{}]",
                    e.id, e.root_path, e.default_branch
                ),
                Err(e) => fail(&format!("register-repo failed: {e}")),
            }
        }
        Some("run") => run_interactive(&core, &args),
        Some("resume") => {
            let Some(sid) = flag(&args, "--session") else {
                fail("resume requires --session <id>");
                return;
            };
            match core.resume_run(&sid) {
                Ok(s) => println!("resumed {sid} → {s:?}"),
                Err(e) => fail(&format!("resume failed: {e}")),
            }
        }
        Some("reattach") => {
            let Some(sid) = flag(&args, "--session") else {
                fail("reattach requires --session <id>");
                return;
            };
            // Subscribe BEFORE resume_run so no events are missed.
            let events = core.subscribe();
            match core.resume_run(&sid) {
                Ok(s) => {
                    println!("reattach {sid} → {s:?}");
                    // resume_run may emit events (SessionFailed, SessionCompleted, etc.) before
                    // returning a terminal status for a crash-recovered session. Drain whatever
                    // is already queued non-blockingly before returning so the operator sees them.
                    if matches!(
                        s,
                        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
                    ) {
                        drain_non_blocking(&events);
                        return;
                    }
                }
                Err(e) => {
                    fail(&format!("reattach failed: {e}"));
                    return;
                }
            }
            drain_events(&events, Some((&core, &sid)));
        }
        Some("cancel") => {
            let Some(sid) = flag(&args, "--session") else {
                fail("cancel requires --session <id>");
                return;
            };
            match core.cancel_run(&sid) {
                Ok(s) => println!("cancelled {sid} → {s:?}"),
                Err(e) => fail(&format!("cancel failed: {e}")),
            }
        }
        Some("launch") => {
            let Some(problem) = flag(&args, "--problem") else {
                fail("launch requires --problem \"...\"");
                return;
            };
            let events = core.subscribe();
            let sid = core.launch(LaunchSpec {
                problem,
                clis: registry_roster(),
                entity_mode: EntityMode::Shared,
                session_id: String::new(),
                human_confirm: HumanConfirm::None,
                repo_ref: None,
                workflow: flag(&args, "--workflow"),
                project_id: flag(&args, "--project"),
                extra_write_roots: Vec::new(),
                // Stub self-test path — no worker, so nothing would read a graph anyway.
                project_graph: None,
            });
            println!(
                "launched {sid} — STUB self-test path (deterministic stub output, no real CLI, no gates); \
                 use `run` for a real governed run"
            );
            drain_events(&events, None);
        }
        _ => {
            eprintln!("{ROOT_USAGE}");
            std::process::exit(2);
        }
    }
}

/// `provision-validator --criterion "..."`: author a deterministic validator for the criterion via the
/// live writer skill (a real `claude` call) and vault it UNAPPROVED, printing its pin. Opens the store
/// directly (sole writer; the actor is NOT spawned for this command) — see the note at the call site.
fn provision_validator_cmd(args: &[String]) {
    let Some(criterion) = flag(args, "--criterion") else {
        fail("provision-validator requires --criterion \"...\"");
        return;
    };
    let mut store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("provision-validator: open store failed: {e}"));
            return;
        }
    };
    let runner = WrappedCliStepRunner::default();
    match wicked_core::provision_validator(&criterion, &runner, &mut store) {
        Ok(pin) => {
            println!("provisioned UNAPPROVED validator, pin: {pin}");
            println!("approve it with:  wicked-core approve-validator --pin {pin}");
        }
        Err(e) => fail(&format!("provision-validator failed: {e}")),
    }
}

/// `approve-validator --pin <pin>`: approve a vaulted (unapproved) validator and print the APPROVED pin
/// the operator drops into a workflow def's `validator_pin`. Opens the store directly (sole writer).
fn approve_validator_cmd(args: &[String]) {
    let Some(pin) = flag(args, "--pin") else {
        fail("approve-validator requires --pin <pin>");
        return;
    };
    let mut store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("approve-validator: open store failed: {e}"));
            return;
        }
    };
    match wicked_core::approve_and_store(&mut store, &pin) {
        Ok(Some(approved)) => {
            println!("approved validator, pin: {approved}");
            println!("put this pin into a workflow def's `validator_pin`: {approved}");
        }
        Ok(None) => fail(&format!(
            "approve-validator: no vaulted validator with pin {pin}"
        )),
        Err(e) => fail(&format!("approve-validator failed: {e}")),
    }
}

/// The workflows overlay dir the planner resolves drop-ins from — `$WICKED_WORKFLOWS_DIR`, else
/// `$HOME/.config/wicked-core/workflows` (mirrors `pipeline::workflow_overlay_dir`). `gate-phase`
/// both READS this (to overlay operator drop-ins onto the built-ins before resolving `--workflow`)
/// and, absent `--out`, WRITES the gated def here so the very next `run --workflow <new-id>` sees it.
fn workflow_overlay_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("WICKED_WORKFLOWS_DIR") {
        return Some(std::path::PathBuf::from(d));
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/wicked-core/workflows"))
}

/// `seed-domain-validators`: seed the DETERMINISTIC, content-pinned coverage validator that the shipped
/// `workflows/domain-extraction.json` gate carries (`validator_pin`) into the vault, so the drop-in
/// actually runs instead of failing closed at plan time. Unlike `provision-validator` (a live LLM writer
/// whose script is nondeterministic and won't reproduce the pin), this vaults + approves the hand-authored
/// `coverage.py --check` port directly, yielding exactly `COVERAGE_VALIDATOR_PIN`. Idempotent
/// (content-addressed). Opens the store as its sole writer (actor not spawned), like the other vault
/// commands.
fn seed_domain_validators_cmd(args: &[String]) {
    let path = store_path(args);
    let mut store = match wicked_apps_core::open_store(Some(&path)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("seed-domain-validators: open store failed: {e}"));
            return;
        }
    };
    match wicked_core::provision_and_approve_coverage_validator(&mut store) {
        Ok(pin) => {
            // Name the DATABASE, not just the pin. The vault is rows in one store, so this command
            // succeeding says nothing about whether the engine can see the result — and when the two
            // disagree it prints the exact pin the failing run asked for while changing nothing that
            // run reads, which is indistinguishable from success (FINDING-066). The path is the only
            // token that makes the mismatch visible without instrumenting anything.
            println!(
                "seeded + approved the domain-extraction coverage validator in {path}, pin: {pin}"
            );
            println!(
                "(matches workflows/domain-extraction.json `validator_pin`; the drop-in now runs gated)"
            );
            println!(
                "NOTE: the vault is per-database. If a running engine still refuses this pin, it \
                 opened a DIFFERENT store than {path} — re-run with `--db <that path>` (a daemon \
                 embedding the engine uses its own state home, e.g. ~/.wicked-crew/core.db)."
            );
        }
        Err(e) => fail(&format!("seed-domain-validators failed: {e}")),
    }
}

/// `gate-phase --workflow <base-id> --phase <phase-id> --criterion "..." [--out <dir>]`: the one path
/// that turns a shipped-style, UNGATED workflow into a GATED one. The built-in feature/bug/migration
/// defs ship with `validator_pin: null` on every phase, so the rev0.4 dual-validator gate is INERT for
/// them — it only engages for a phase carrying a `validator_pin`. This command closes that: it loads the
/// base def, AUTHORS + APPROVES a deterministic validator for `--criterion` (a live `claude` call via the
/// writer skill, exactly like `provision-validator`), PINS the approved pin onto the named phase, and
/// writes the modified def as a NEW drop-in workflow JSON (fresh id, so it never clobbers the built-in)
/// into the workflows overlay dir. The operator then runs `run --workflow <new-id>` and the gate engages.
///
/// Opens the store directly as its SOLE writer (the actor is NOT spawned — same reason as
/// provision-validator/approve-validator). Fail-closed on an unknown workflow id or an unknown phase id
/// (both name the valid choices).
fn gate_phase_cmd(args: &[String]) {
    let Some(workflow) = flag(args, "--workflow") else {
        fail("gate-phase requires --workflow <base-id>");
        return;
    };
    let Some(phase) = flag(args, "--phase") else {
        fail("gate-phase requires --phase <phase-id>");
        return;
    };
    let Some(criterion) = flag(args, "--criterion") else {
        fail("gate-phase requires --criterion \"...\"");
        return;
    };

    // 1. Resolve the base WorkflowDef: the built-ins overlaid with operator drop-ins (the same seam the
    //    planner resolves against), so `--workflow` can name a shipped OR a previously dropped-in workflow.
    let mut reg = WorkflowRegistry::with_defaults();
    if let Some(dir) = workflow_overlay_dir() {
        if let Err(e) = reg.load_dir(&dir) {
            eprintln!(
                "gate-phase: workflow overlay {} failed to load ({e}); using built-ins only",
                dir.display()
            );
        }
    }
    let Some(base) = reg.get(&workflow) else {
        fail(&format!(
            "gate-phase: unknown workflow `{workflow}` — known workflows: {}",
            reg.ids().join(", ")
        ));
        return;
    };
    let mut def = base.clone();

    // 2. Fail-closed on an unknown phase id, NAMING the valid phases so the operator can correct it.
    if !def.phases.iter().any(|p| p.id == phase) {
        let valid: Vec<&str> = def.phases.iter().map(|p| p.id.as_str()).collect();
        fail(&format!(
            "gate-phase: workflow `{workflow}` has no phase `{phase}` — valid phases: {}",
            valid.join(", ")
        ));
        return;
    }

    // 3. AUTHOR + APPROVE a validator for the criterion (live `claude`), as the sole store writer.
    let mut store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("gate-phase: open store failed: {e}"));
            return;
        }
    };
    let runner = WrappedCliStepRunner::default();
    let unapproved = match wicked_core::provision_validator(&criterion, &runner, &mut store) {
        Ok(p) => p,
        Err(e) => {
            fail(&format!("gate-phase: authoring the validator failed: {e}"));
            return;
        }
    };
    let approved = match wicked_core::approve_and_store(&mut store, &unapproved) {
        Ok(Some(p)) => p,
        Ok(None) => {
            fail(&format!(
                "gate-phase: the just-authored validator (pin {unapproved}) was not found in the \
                 vault to approve"
            ));
            return;
        }
        Err(e) => {
            fail(&format!("gate-phase: approving the validator failed: {e}"));
            return;
        }
    };

    // 4. PIN the approved validator onto the phase and RE-ID the def so the drop-in never clobbers the
    //    built-in (a fresh id the operator selects with `run --workflow <new-id>`).
    let new_id = format!("{phase}-gated-{workflow}");
    def.id = new_id.clone();
    for p in def.phases.iter_mut() {
        if p.id == phase {
            p.validator_pin = Some(approved.clone());
        }
    }

    // 5. WRITE the gated def as a drop-in JSON: `--out` wins, else the resolved overlay dir (so the
    //    very next `run --workflow <new-id>` picks it up without any extra config).
    let Some(out_dir) = flag(args, "--out")
        .map(std::path::PathBuf::from)
        .or_else(workflow_overlay_dir)
    else {
        fail(
            "gate-phase: no output dir — pass --out <dir>, or set $WICKED_WORKFLOWS_DIR / $HOME so the \
             workflows overlay dir resolves",
        );
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        fail(&format!(
            "gate-phase: creating {} failed: {e}",
            out_dir.display()
        ));
        return;
    }
    let out_path = out_dir.join(format!("{new_id}.json"));
    let json = match serde_json::to_string_pretty(&def) {
        Ok(j) => j,
        Err(e) => {
            fail(&format!(
                "gate-phase: serializing the gated def failed: {e}"
            ));
            return;
        }
    };
    if let Err(e) = std::fs::write(&out_path, &json) {
        fail(&format!(
            "gate-phase: writing {} failed: {e}",
            out_path.display()
        ));
        return;
    }

    println!("gated workflow written: {}", out_path.display());
    println!("  new workflow id: {new_id}");
    println!("  phase `{phase}` now pins APPROVED validator: {approved}");
    println!(
        "the dual-validator gate now ENGAGES for phase `{phase}`. run it with:\n  \
         wicked-core run --problem \"...\" --workflow {new_id} --repo <id>"
    );
}

/// An interactive governed run: stream events and, at each human-confirm gate, prompt the operator
/// on stdin (a = approve, r = reject) and resolve the gate.
fn run_interactive(core: &Core, args: &[String]) {
    let Some(problem) = flag(args, "--problem") else {
        fail("run requires --problem \"...\"");
        return;
    };
    let repo_ref = flag(args, "--repo");
    let session_id = flag(args, "--session").unwrap_or_default();
    // --clis <csv>: restrict distribution to a named subset of the roster (e.g. "claude").
    // Without this flag the full registry roster is used (council convenes all seats).
    let clis = if let Some(csv) = flag(args, "--clis") {
        let keys: std::collections::HashSet<String> =
            csv.split(',').map(|s| s.trim().to_string()).collect();
        let filtered: Vec<_> = registry_roster()
            .into_iter()
            .filter(|c| keys.contains(&c.key))
            .collect();
        if filtered.is_empty() {
            fail(&format!("--clis '{csv}' matched no roster seats"));
            return;
        }
        filtered
    } else {
        registry_roster()
    };
    let human_confirm = match parse_confirm(args) {
        Ok(hc) => hc,
        Err(e) => {
            fail(&format!("invalid --confirm: {e}"));
            return;
        }
    };
    let events = core.subscribe();
    let run_id = match core.launch_run(LaunchSpec {
        problem,
        clis,
        entity_mode: EntityMode::Shared,
        session_id,
        human_confirm,
        repo_ref,
        workflow: flag(args, "--workflow"),
        project_id: flag(args, "--project"),
        extra_write_roots: Vec::new(),
        // `--project` files the run; it does NOT bind a project graph, and this CLI deliberately
        // has no flag that would. The graph's location, its membership and its estate labels are
        // the launcher's to know (see `project::ProjectGraphBinding`), and an operator CLI that
        // guessed `~/.wicked-crew/project-graphs/…` would be the sixth spelling of a path the
        // engine does not own. A run launched here gets its own repo's graph; launch through
        // crew's API to get the project's.
        project_graph: None,
    }) {
        Ok(id) => id,
        Err(e) => {
            fail(&format!("run failed: {e}"));
            return;
        }
    };
    println!("running {run_id}");
    drain_events(&events, Some((core, &run_id)));
}

/// Drain all events already queued in the channel without blocking. Used before an early-return
/// when `resume_run` signals a terminal state — it may have dispatched events (SessionFailed,
/// SessionCompleted, etc.) before returning, and we want the operator to see them.
fn drain_non_blocking(events: &std::sync::mpsc::Receiver<CoreEvent>) {
    while let Ok(ev) = events.try_recv() {
        println!("  {ev:?}");
    }
}

/// Print every event until the run reaches a terminal state. If `gate` is set, prompt the operator
/// at each `AwaitingHuman` and resolve it via `confirm_gate`.
fn drain_events(events: &std::sync::mpsc::Receiver<CoreEvent>, gate: Option<(&Core, &str)>) {
    // When `gate` carries a session id, only treat terminal events for *that* session as
    // completion.  Events from other concurrent sessions (campaigns, terminal sessions, etc.)
    // are printed but do not break the loop or trigger gate responses.
    let is_mine = |s: &str| gate.map(|(_, id)| id == s).unwrap_or(true);
    loop {
        match events.recv_timeout(Duration::from_secs(3600)) {
            Ok(ev) => {
                println!("  {ev:?}");
                match &ev {
                    CoreEvent::AwaitingHuman {
                        session, prompt, ..
                    } if is_mine(session) => {
                        if let Some((core, run_id)) = gate {
                            let decision = prompt_decision(prompt);
                            match core.confirm_gate(run_id, decision) {
                                Ok(s) => println!("  → gate resolved: {s:?}"),
                                Err(e) => {
                                    fail(&format!("confirm_gate failed: {e}"));
                                    return;
                                }
                            }
                        }
                    }
                    CoreEvent::SessionCompleted { session }
                    | CoreEvent::RunCancelled { session }
                    | CoreEvent::SessionFailed { session, .. }
                        if is_mine(session) =>
                    {
                        break
                    }
                    CoreEvent::Error { session, .. }
                        if session.as_deref().map(is_mine).unwrap_or(true) =>
                    {
                        break
                    }
                    _ => {}
                }
            }
            Err(_) => {
                fail("timed out waiting for the run");
                return;
            }
        }
    }
}

/// Prompt the operator on stdin for a gate decision (a = approve, r = reject; default approve).
fn prompt_decision(prompt: &str) -> HumanDecision {
    println!("  ❓ {prompt}\n  [a]pprove / [r]eject ? ");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    match line.trim().chars().next() {
        Some('r') | Some('R') => HumanDecision::Reject,
        _ => HumanDecision::Approve { amend: None },
    }
}

fn print_status(core: &Core) {
    match core.sessions_detail() {
        Ok(views) if views.is_empty() => println!("(no sessions)"),
        Ok(views) => {
            for v in views {
                let done = v
                    .units
                    .iter()
                    .filter(|u| matches!(u.status, wicked_core::UnitStatus::Done))
                    .count();
                println!(
                    "{} [{:?}] {}/{} units done",
                    v.session.id,
                    v.session.status,
                    done,
                    v.units.len()
                );
            }
        }
        Err(e) => fail(&format!("status failed: {e}")),
    }
}

/// `wicked-core domain-graph` — translate the annotated estate graph into a `requirements_graph.json`
/// domain model (DES-OUTGOV-001 PR-D). Reads the front-half coverage report, gates on coverage == 1.0
/// (FAIL-CLOSED — refuses to translate an unannotated graph), builds the model (functional / package-
/// dir grouping, M5), and writes the artifact. Like the other pre-`Core::spawn` subcommands it opens
/// the store directly for a brief read and never spawns the actor.
///
/// STORE-BOUND coverage (core#25): front-half coverage is now RECOMPUTED directly from the store as the
/// PRIMARY source — the gate no longer trusts a separate file. A supplied `--coverage <file>` is an
/// optional cross-check that must AGREE with the recompute (fail-closed on disagreement). This closes the
/// trust-boundary hole (a stale report can no longer green-light a different graph) that the prior
/// increment left as a follow-on.
fn domain_graph_cmd(args: &[String]) {
    let out_path = flag(args, "--out")
        .unwrap_or_else(|| ".wicked-estate/requirements/requirements_graph.json".to_string());
    // The schema pins metadata.schema_version to const "1.0.0" — a consumer rejects a version it has
    // no validator for, so the emitted document must carry exactly this.
    let schema_version = flag(args, "--schema-version").unwrap_or_else(|| "1.0.0".to_string());

    let mut store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("domain-graph: open store failed: {e}"));
            return;
        }
    };

    // Front-half coverage is RECOMPUTED from the store (PRIMARY — the store is the source of truth, not a
    // trusted external file). A supplied `--coverage <file>` is an optional CROSS-CHECK that must AGREE;
    // an absent/unsupplied file is NOT an error (recompute stands). (DES-OUTGOV-005 decision #4.)
    let coverage = match wicked_governance::recompute_front_half_coverage(&store) {
        Ok(c) => c,
        Err(e) => {
            fail(&format!("domain-graph: coverage recompute failed: {e}"));
            return;
        }
    };
    if let Some(coverage_path) = flag(args, "--coverage") {
        match std::fs::read_to_string(&coverage_path) {
            Ok(s) => match serde_json::from_str::<wicked_governance::CoverageReport>(&s) {
                Ok(file) => {
                    // "Must agree" = every EXACT integer count matches the store recompute (unaccounted is
                    // the gate field, but a mismatch in any count means a different/stale graph); `coverage`
                    // is a rounded ratio, so compare it only with a generous tolerance (not f64::EPSILON,
                    // which spuriously fails on JSON-parse/round drift).
                    // Every EXACT field: total (all nodes — a different graph) + the four bucket counts +
                    // resolve_threshold (a different config re-buckets resolved/risk). With these + the
                    // hole set matching, the derived float ratios (coverage/resolved_rate/mean_confidence)
                    // are determined, so this is a COMPLETE agreement check.
                    let ints_disagree = file.total != coverage.total
                        || file.behavior_bearing != coverage.behavior_bearing
                        || file.resolved != coverage.resolved
                        || file.risk_flagged != coverage.risk_flagged
                        || file.unaccounted != coverage.unaccounted
                        || (file.resolve_threshold - coverage.resolve_threshold).abs() > 1e-9;
                    // Also compare the actual HOLE SET (unaccounted symbol_ids) — a file whose top-level
                    // counts match but whose holes are a different set is a stale/different graph.
                    let hole_set = |r: &wicked_governance::CoverageReport| {
                        let mut v: Vec<&str> = r
                            .unaccounted_nodes
                            .iter()
                            .map(|n| n.symbol_id.as_str())
                            .collect();
                        v.sort_unstable();
                        v.join(",")
                    };
                    let holes_disagree = hole_set(&file) != hole_set(&coverage);
                    if ints_disagree
                        || holes_disagree
                        || (file.coverage - coverage.coverage).abs() > 1e-4
                    {
                        fail(&format!(
                            "domain-graph: supplied --coverage {coverage_path} DISAGREES with the store \
                             recompute (file coverage={:.4}/unaccounted={}, store coverage={:.4}/unaccounted={}) \
                             — refusing (fail-closed)",
                            file.coverage, file.unaccounted, coverage.coverage, coverage.unaccounted
                        ));
                        return;
                    }
                }
                Err(e) => {
                    // A deserialize error here is EITHER invalid JSON OR a valid-JSON-but-wrong-shape file
                    // (deny_unknown_fields rejects a stray/extra key) — say so rather than only "invalid JSON".
                    fail(&format!(
                        "domain-graph: supplied --coverage {coverage_path} does not match the coverage \
                         schema (invalid JSON or an unexpected/missing field): {e}"
                    ));
                    return;
                }
            },
            Err(e) => {
                fail(&format!(
                    "domain-graph: supplied --coverage {coverage_path} cannot be read: {e}"
                ));
                return;
            }
        }
    }

    // Fail-closed: `build_domain_model` bails when coverage < 1.0 (never translates a partial graph) AND
    // recomputes internally, so a store hole denies even if the passed report claimed 1.0.
    let model = match wicked_governance::build_domain_model(&store, &coverage, &schema_version) {
        Ok(m) => m,
        Err(e) => {
            fail(&format!("domain-graph: {e}"));
            return;
        }
    };

    // The domain graph belongs IN the estate DB, not only as a JSON file (operator directive; the
    // locked "domain data lives in estate's graph" direction). Persist the assembled model as
    // nodes/edges into the SAME store it was built from — this is the source of truth; the JSON
    // --out below is an optional export. Fail-closed: if the graph cannot be persisted, the phase
    // has produced no durable evidence, so the command must not report success.
    match wicked_governance::persist_domain_model(&mut store, &model) {
        Ok((nodes, edges)) => {
            eprintln!(
                "domain-graph: persisted {nodes} node(s) + {edges} edge(s) into the estate store"
            );
        }
        Err(e) => {
            fail(&format!("domain-graph: persist to store failed: {e}"));
            return;
        }
    }

    if let Some(parent) = std::path::Path::new(&out_path).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            fail(&format!(
                "domain-graph: cannot create {}: {e}",
                parent.display()
            ));
            return;
        }
    }
    let json = serde_json::to_string_pretty(&model).expect("DomainModel serializes to JSON");
    match std::fs::write(&out_path, json) {
        Ok(()) => println!(
            "domain-graph: wrote {} domain(s) → {out_path}",
            model.domains.len()
        ),
        Err(e) => fail(&format!("domain-graph: cannot write {out_path}: {e}")),
    }
}

/// `wicked-core coverage [--out F]` — recompute the front-half coverage report DIRECTLY from the store
/// and emit `coverage-report.json` (schema-exact). `--out` defaults to a bare `coverage-report.json` in
/// the cwd so the shipped grep validator (which reads that path from the phase worktree) finds it
/// (DES-OUTGOV-005 decision #4). Opens the store directly for a brief read; never spawns the actor.
fn coverage_cmd(args: &[String]) {
    let json_stdout = args.iter().any(|a| a == "--json");
    let store = match wicked_apps_core::open_store(Some(&store_path(args))) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("coverage: open store failed: {e}"));
            return;
        }
    };
    let report = match wicked_governance::recompute_front_half_coverage(&store) {
        Ok(r) => r,
        Err(e) => {
            fail(&format!("coverage: recompute failed: {e}"));
            return;
        }
    };
    let json = serde_json::to_string_pretty(&report).expect("CoverageReport serializes to JSON");
    if json_stdout {
        // --json: emit to stdout so callers (e.g. wicked-crew) don't need a temp file.
        println!("{json}");
        return;
    }
    let out_path = flag(args, "--out").unwrap_or_else(|| "coverage-report.json".to_string());
    if let Some(parent) = std::path::Path::new(&out_path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                fail(&format!(
                    "coverage: cannot create {}: {e}",
                    parent.display()
                ));
                return;
            }
        }
    }
    match std::fs::write(&out_path, json) {
        Ok(()) => println!(
            "coverage: {:.4} ({} behavior-bearing, {} unaccounted) → {out_path}",
            report.coverage, report.behavior_bearing, report.unaccounted
        ),
        Err(e) => fail(&format!("coverage: cannot write {out_path}: {e}")),
    }
}

/// `wicked-core rules ingest <dir> [--db F]` — populate the store with governance POLICIES (the
/// deterministic deny path) + CONFORMANCE RULES (recall→obligation), so the output guardrail
/// (`output-gate-hook`) has real rules to enforce (core#26, DES-OUTGOV-006). Layout:
///   <dir>/policies/*.json  — each a `Policy` or `[Policy]` → register_policy (deny on a matching output)
///   <dir>/rules/*.json     — conformance-rule bundles → ingest_from + register_rule (recall→obligation)
///   <dir>/**/*.md          — frontmattered markdown rule docs (AW-3 `MarkdownAdapter`: YAML
///                            frontmatter + `## Rules` items) → the SAME ingest_from/normalize_bundle
///                            path + register_rule; a `.md` without a leading `---` fence is not a
///                            rule doc and is not claimed
/// FAIL-LOUD: a malformed/unreadable file errors (never a partial silent load — a malformed markdown
/// doc names its path + reason); a rule id colliding across the JSON and markdown lanes errors (both
/// map to `conformance_rule/<id>`, so the later write would silently clobber the earlier); an EMPTY
/// effective load (0 policies + 0 rules) errors (a silent no-op population would read as "governed"
/// while enforcing nothing). A missing `policies/` or `rules/` subdir is tolerated (a ruleset may
/// carry only one kind). A successful ingest also registers the 4 governance schema-document nodes
/// (keyed by `$id`) so rules can reference the contract they were validated under.
fn rules_ingest_cmd(args: &[String]) {
    // The <dir> is the first NON-FLAG token after `rules ingest` (index 3+). Only the KNOWN value-taking
    // flags (`--db`/`--dir`) consume a following value; any other `--token` is skipped alone (not blindly
    // `i += 2`, which would swallow the dir after a bare flag). So `rules ingest --db x /dir` and
    // `rules ingest /dir --db x` both resolve the dir.
    const VALUE_FLAGS: &[&str] = &["--db", "--dir"];
    let positional_dir = || -> Option<String> {
        let mut i = 3;
        while i < args.len() {
            let a = &args[i];
            if a.starts_with("--") {
                // A value-flag consumes the NEXT token only if it is a real value (not another flag) — so
                // a missing value (`--db --dir /dir`) doesn't swallow the following flag.
                let has_value = VALUE_FLAGS.contains(&a.as_str())
                    && args.get(i + 1).is_some_and(|v| !v.starts_with("--"));
                i += if has_value { 2 } else { 1 };
            } else {
                return Some(a.clone());
            }
        }
        None
    };
    let dir = match flag(args, "--dir").or_else(positional_dir) {
        Some(d) => d,
        None => {
            fail("rules ingest requires a directory: wicked-core rules ingest <dir> [--db F]");
            return;
        }
    };
    // Symmetric to the --db guard below: `--dir --db x` makes flag("--dir") return "--db" (a flag-shaped
    // dir). Reject it loudly rather than reporting a confusing "empty population" for a non-existent dir.
    if dir.starts_with("--") {
        fail(&format!(
            "rules ingest: --dir has no value (resolved to {dir:?}) — refusing a flag-shaped directory"
        ));
        return;
    }
    // Guard a missing value-flag value from misdirecting the store: `--db --dir x` makes flag("--db")
    // return "--dir", which would ingest into a stray file named "--dir" and REPORT SUCCESS while the real
    // run's store is untouched (a populated-the-wrong-store fail-open). Reject a flag-shaped db path.
    let resolved_db = store_path(args);
    if resolved_db.starts_with("--") {
        fail(&format!(
            "rules ingest: --db has no value (resolved to {resolved_db:?}) — refusing to ingest into a \
             flag-shaped store path"
        ));
        return;
    }
    let mut store = match wicked_apps_core::open_store(Some(&resolved_db)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("rules ingest: open store failed: {e}"));
            return;
        }
    };

    // STEERING migration-on-open: ingest is the population path that already holds the store as a
    // brief sole writer, so it heals any legacy `Other(POLICY)` rows into unified steering rules
    // FIRST (one-time, idempotent — a migrated store reports nothing). Conflicts/skips are LOUD:
    // an unmigrated enforcement row still enforces (select's read-time fallback), but the operator
    // must know the unified listing does not carry it yet.
    match wicked_governance::migrate_policies_to_steering(&mut store) {
        Ok(report) => {
            if !report.migrated.is_empty() {
                println!(
                    "rules ingest: migrated {} legacy policy row(s) into steering rules: {}",
                    report.migrated.len(),
                    report.migrated.join(", ")
                );
            }
            for id in &report.conflicts {
                eprintln!(
                    "rules ingest: WARNING policy {id:?} conflicts with an existing recall-only \
                     steering rule at the same id — NOT migrated (still enforced via the legacy \
                     row); rename the policy or retire the rule, then re-ingest"
                );
            }
            for skipped in &report.skipped {
                eprintln!(
                    "rules ingest: WARNING legacy policy row skipped by the steering migration: \
                     {skipped}"
                );
            }
        }
        Err(e) => {
            fail(&format!("rules ingest: steering migration failed: {e}"));
            return;
        }
    }

    let root = std::path::Path::new(&dir);
    let mut n_policies = 0usize;
    let mut n_rules = 0usize;
    // Rule ids seen across BOTH conformance lanes (JSON bundles + markdown docs). ingest_from
    // enforces INV-C3 within each adapter; this set extends it across the two, because a
    // cross-lane duplicate would silently overwrite at register (same `conformance_rule/<id>`).
    let mut seen_rule_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Conformance rules (recall→obligation), JSON lane.
    let rules_dir = root.join("rules");
    if rules_dir.is_dir() {
        let adapter = wicked_governance::FilesystemAdapter::new(&rules_dir);
        match wicked_governance::ingest_from(&adapter) {
            Ok(rules) => {
                for r in &rules {
                    seen_rule_ids.insert(r.id.clone());
                    if let Err(e) = wicked_governance::register_rule(&mut store, r) {
                        fail(&format!(
                            "rules ingest: register conformance rule {} failed: {e}",
                            r.id
                        ));
                        return;
                    }
                    n_rules += 1;
                }
            }
            Err(e) => {
                fail(&format!(
                    "rules ingest: reading conformance rules under {rules_dir:?}: {e}"
                ));
                return;
            }
        }
    }

    // Conformance rules, markdown lane (AW-3): frontmattered `*.md` docs anywhere under <dir>,
    // through the SAME normalize_bundle fail-closed path (one parse convention, no second path).
    {
        let adapter = wicked_governance::MarkdownAdapter::new(root);
        match wicked_governance::ingest_from(&adapter) {
            Ok(rules) => {
                for r in &rules {
                    if !seen_rule_ids.insert(r.id.clone()) {
                        fail(&format!(
                            "rules ingest: rule id {:?} appears in BOTH a rules/*.json bundle and a \
                             markdown doc ({}) — the later write would silently overwrite the earlier \
                             at conformance_rule/<id>; refusing (fail-loud)",
                            r.id,
                            r.provenance.reference.as_deref().unwrap_or("?")
                        ));
                        return;
                    }
                    if let Err(e) = wicked_governance::register_rule(&mut store, r) {
                        fail(&format!(
                            "rules ingest: register conformance rule {} (markdown) failed: {e}",
                            r.id
                        ));
                        return;
                    }
                    n_rules += 1;
                }
            }
            Err(e) => {
                fail(&format!(
                    "rules ingest: reading markdown rule docs under {root:?}: {e}"
                ));
                return;
            }
        }
    }

    // Governance policies (the deterministic deny path).
    let policies_dir = root.join("policies");
    if policies_dir.is_dir() {
        // Enumerate explicitly, PROPAGATING a mid-readdir fault (never `.ok()`-drop it — a silent skip
        // would truncate the DENY set and fail governance OPEN, the exact hazard the conformance adapter
        // was written to avoid).
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let rd = match std::fs::read_dir(&policies_dir) {
            Ok(rd) => rd,
            Err(e) => {
                fail(&format!("rules ingest: cannot read {policies_dir:?}: {e}"));
                return;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    fail(&format!(
                        "rules ingest: cannot enumerate {policies_dir:?}: {e}"
                    ));
                    return;
                }
            };
            let p = entry.path();
            if p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("json"))
            {
                files.push(p);
            }
        }
        files.sort(); // deterministic ingest order
                      // INV-C3-style dedup on the DENY path: a duplicate policy id would silently OVERWRITE at register
                      // (both map to `policy/<id>`; a Deny could be clobbered by a weaker Allow) while the count
                      // over-reports. Fail loud on a collision across the whole `policies/` bundle.
        let mut seen_policy_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for p in files {
            let text = match std::fs::read_to_string(&p) {
                Ok(t) => t,
                Err(e) => {
                    fail(&format!("rules ingest: cannot read {p:?}: {e}"));
                    return;
                }
            };
            // A file is either a single Policy object or an array of Policies. Dispatch on the JSON SHAPE
            // (first non-whitespace char) so a malformed object surfaces the SPECIFIC Policy error (a
            // missing/typo'd field) rather than a misleading "expected a sequence" from a fallback
            // array-parse.
            let is_array = text.trim_start().starts_with('[');
            let parsed: Result<Vec<wicked_governance::Policy>, _> = if is_array {
                serde_json::from_str::<Vec<wicked_governance::Policy>>(&text)
            } else {
                serde_json::from_str::<wicked_governance::Policy>(&text).map(|p| vec![p])
            };
            let policies = match parsed {
                Ok(ps) => ps,
                Err(e) => {
                    let shape = if is_array { "[Policy]" } else { "Policy" };
                    fail(&format!("rules ingest: {p:?} is not a valid {shape}: {e}"));
                    return;
                }
            };
            for pol in &policies {
                if !seen_policy_ids.insert(pol.id.clone()) {
                    fail(&format!(
                        "rules ingest: duplicate policy id {:?} across policies/*.json — a later policy \
                         would silently overwrite an earlier one at register (both map to policy/<id>, \
                         e.g. a Deny replaced by an Allow); refusing (fail-loud)",
                        pol.id
                    ));
                    return;
                }
                if let Err(e) = wicked_governance::register_policy(&mut store, pol) {
                    fail(&format!(
                        "rules ingest: register policy {} failed: {e}",
                        pol.id
                    ));
                    return;
                }
                n_policies += 1;
            }
        }
    }

    if n_policies == 0 && n_rules == 0 {
        fail(&format!(
            "rules ingest: NO policies or conformance rules found under {dir} (expected \
             <dir>/policies/*.json, <dir>/rules/*.json, and/or frontmattered markdown rule docs \
             with a `## Rules` section) — refusing an empty population (fail-loud)"
        ));
        return;
    }
    // Doctrine RuleSet grouping (AW-13 / arch-R9): frontmatter `domain:` selects the parent —
    // mint one native RuleSet node per domain + Contains membership edges, AFTER the member
    // rules landed above (a membership edge must never target a not-yet-written node).
    let groupings = match wicked_governance::MarkdownAdapter::new(root).groupings() {
        Ok(g) => g,
        Err(e) => {
            fail(&format!(
                "rules ingest: reading RuleSet groupings under {root:?}: {e}"
            ));
            return;
        }
    };
    let (n_rulesets, n_memberships) =
        match wicked_governance::register_rule_sets(&mut store, &groupings) {
            Ok(counts) => counts,
            Err(e) => {
                fail(&format!("rules ingest: register RuleSets failed: {e}"));
                return;
            }
        };
    // A successful ingest also registers the governance schema-document nodes (one per schema
    // file, keyed by $id — schemas/README.md AW-3 seam), so the store's rules can reference the
    // contract version they were validated under. After the empty-population check: a refused
    // ingest must not leave a half-populated store.
    let n_schemas = match wicked_governance::register_schema_nodes(&mut store) {
        Ok(n) => n,
        Err(e) => {
            fail(&format!("rules ingest: register schema nodes failed: {e}"));
            return;
        }
    };
    println!(
        "rules ingest: registered {n_policies} policies + {n_rules} conformance rules \
         (+ {n_rulesets} RuleSets / {n_memberships} memberships + {n_schemas} schema nodes) \
         from {dir}"
    );
}

/// Every value of a repeatable `--flag` (in argv order).
fn flag_all(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Lexically normalize a path (resolve `.`/`..` without touching the filesystem — lane dbs may not
/// exist yet), absolutized against the cwd.
fn normalize_lexical(path: &str) -> std::path::PathBuf {
    use std::path::Component;
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(p)
    };
    let mut out = std::path::PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The crew daemon's state home when `path` resolves inside it, else `None`. A store under
/// `~/.wicked-crew` is daemon-held BY LOCATION: the single-writer actor owns it, and a second
/// OS-process writer is the exact two-writer hazard the gate-hook architecture removed. This is a
/// guard against the obvious accident, not a security boundary — the contract (never CLI against a
/// daemon store) is DES-OUTGOV-008's.
fn daemon_fenced(path: &str) -> Option<String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)?;
    let fence = normalize_lexical(&home.join(".wicked-crew").to_string_lossy());
    let target = normalize_lexical(path);
    target
        .starts_with(&fence)
        .then(|| fence.to_string_lossy().into_owned())
}

/// `wicked-core rules fanout <dir> …` — the AW-5 fan-out contract across the deliberate store
/// split (arch-R3; decision record `.product/DES-OUTGOV-008-fanout-placement.md`). One ruleset
/// (`rules ingest` layout) fans out to (a) the enforcement store the gate hook reads, (b) every
/// discovery graph the workers' estate MCP binds (one per live repo under `--scope workspace`,
/// the AW-6 replicate-to-every-repo decision), and (c) a knowledge rationale chunk per rule.
/// Every cli lane is smoke-verified against a FRESH handle on the same `--db` a worker is handed;
/// the manifest keyed on PAT-/POL- ids is written only when every cli lane verified (fail-loud).
///
/// A daemon-held store is NEVER CLI-written: `--enforcement-crew-api` records the pending
/// transport in the manifest and emits the `{policies, rules}` payload for
/// `POST /api/v1/governance/{policies,rules}` next to the manifest; any lane path under
/// `~/.wicked-crew` is refused outright.
fn rules_fanout_cmd(args: &[String]) {
    // The <dir> is the first NON-FLAG token after `rules fanout` (index 3+); only KNOWN
    // value-taking flags consume a following value (same shape as `rules ingest`).
    const VALUE_FLAGS: &[&str] = &[
        "--dir",
        "--manifest",
        "--scope",
        "--enforcement-db",
        "--enforcement-crew-api",
        "--discovery-db",
        "--knowledge-db",
        "--knowledge-scope",
    ];
    let positional_dir = || -> Option<String> {
        let mut i = 3;
        while i < args.len() {
            let a = &args[i];
            if a.starts_with("--") {
                let has_value = VALUE_FLAGS.contains(&a.as_str())
                    && args.get(i + 1).is_some_and(|v| !v.starts_with("--"));
                i += if has_value { 2 } else { 1 };
            } else {
                return Some(a.clone());
            }
        }
        None
    };
    let dir = match flag(args, "--dir").or_else(positional_dir) {
        Some(d) => d,
        None => {
            fail(
                "rules fanout requires a ruleset directory: wicked-core rules fanout <dir> \
                 (--enforcement-db F | --enforcement-crew-api URL) --discovery-db F... \
                 --knowledge-db F... [--scope repo|workspace] [--manifest OUT.json]",
            );
            return;
        }
    };
    // Reject flag-shaped values (a missing value makes `flag` return the NEXT flag) — same guards
    // as `rules ingest`, because a mis-parsed lane path writes the wrong store and reports success.
    for name in VALUE_FLAGS {
        for v in flag_all(args, name) {
            if v.starts_with("--") {
                fail(&format!(
                    "rules fanout: {name} has no value (resolved to {v:?}) — refusing a \
                     flag-shaped argument"
                ));
                return;
            }
        }
    }
    if dir.starts_with("--") {
        fail(&format!(
            "rules fanout: --dir has no value (resolved to {dir:?}) — refusing a flag-shaped \
             directory"
        ));
        return;
    }

    let scope = match flag(args, "--scope").as_deref() {
        None | Some("repo") => wicked_governance::FanoutScope::Repo,
        Some("workspace") => wicked_governance::FanoutScope::Workspace,
        Some(other) => {
            fail(&format!(
                "rules fanout: --scope must be `repo` or `workspace`, got {other:?} (fail-loud — \
                 a typo'd scope must not silently narrow a workspace fan-out)"
            ));
            return;
        }
    };

    let enforcement_db = flag(args, "--enforcement-db");
    let enforcement_api = flag(args, "--enforcement-crew-api");
    let enforcement = match (&enforcement_db, &enforcement_api) {
        (Some(db), None) => wicked_governance::EnforcementTarget::Cli { db: db.clone() },
        (None, Some(url)) => wicked_governance::EnforcementTarget::CrewApi { url: url.clone() },
        (Some(_), Some(_)) => {
            fail(
                "rules fanout: --enforcement-db and --enforcement-crew-api are mutually \
                 exclusive — a store is either offline (cli-written) or daemon-held (crew API), \
                 never both",
            );
            return;
        }
        (None, None) => {
            fail(
                "rules fanout: an enforcement target is required — --enforcement-db <path> for an \
                 offline store, or --enforcement-crew-api <url> for a store a crew daemon holds",
            );
            return;
        }
    };

    let discovery_dbs = flag_all(args, "--discovery-db");
    let knowledge_dbs = flag_all(args, "--knowledge-db");

    // NEVER the CLI against a daemon-held store — in ANY lane. The enforcement lane has the
    // crew-api transport as its sanctioned alternative; a discovery/knowledge path under the
    // daemon home is simply a wrong target.
    let mut fenced: Vec<&String> = Vec::new();
    if let Some(db) = &enforcement_db {
        fenced.push(db);
    }
    fenced.extend(discovery_dbs.iter());
    fenced.extend(knowledge_dbs.iter());
    for path in fenced {
        if let Some(fence) = daemon_fenced(path) {
            fail(&format!(
                "rules fanout: {path:?} resolves under the crew daemon's state home ({fence}) — a \
                 daemon-held store is never CLI-written (single-writer invariant); use \
                 --enforcement-crew-api and POST the emitted payload instead"
            ));
            return;
        }
    }

    let targets = wicked_governance::FanoutTargets {
        scope,
        enforcement,
        discovery_dbs,
        knowledge_dbs,
        knowledge_scope: flag(args, "--knowledge-scope")
            .unwrap_or_else(|| "wiki:governance".to_string()),
    };

    let load = match wicked_governance::load_ruleset(std::path::Path::new(&dir)) {
        Ok(l) => l,
        Err(e) => {
            fail(&format!("rules fanout: {e}"));
            return;
        }
    };
    let manifest = match wicked_governance::fanout(&load, &targets, &dir, now_secs()) {
        Ok(m) => m,
        Err(e) => {
            fail(&format!("rules fanout: {e}"));
            return;
        }
    };

    let manifest_path = flag(args, "--manifest").unwrap_or_else(|| "fanout-manifest.json".into());
    let json = match serde_json::to_string_pretty(&manifest) {
        Ok(j) => j,
        Err(e) => {
            fail(&format!("rules fanout: serialize manifest: {e}"));
            return;
        }
    };
    if let Err(e) = std::fs::write(&manifest_path, json) {
        fail(&format!("rules fanout: cannot write {manifest_path}: {e}"));
        return;
    }

    println!(
        "rules fanout: {} conformance rules + {} policies from {dir}",
        manifest.rules.len(),
        manifest.policies.len()
    );
    println!(
        "  enforcement [{}] {} — {}",
        manifest.enforcement.transport,
        manifest.enforcement.target,
        if manifest.enforcement.verified {
            "VERIFIED"
        } else {
            "PENDING (crew API)"
        }
    );
    for lane in &manifest.discovery {
        println!("  discovery   {} — VERIFIED", lane.db);
    }
    for lane in &manifest.knowledge {
        println!("  knowledge   {} — VERIFIED", lane.db);
    }
    println!("  manifest → {manifest_path}");

    // A daemon-held enforcement store gets its copies over the crew API: emit the payload the
    // operator POSTs, next to the manifest, so "the enforcement copy" exists concretely even
    // though this process could not write it.
    if let wicked_governance::EnforcementTarget::CrewApi { url } = &targets.enforcement {
        let payload_path = format!("{manifest_path}.crew-payload.json");
        let payload = serde_json::json!({
            "policies": load.policies,
            "rules": load.rules,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(j) => {
                if let Err(e) = std::fs::write(&payload_path, j) {
                    fail(&format!("rules fanout: cannot write {payload_path}: {e}"));
                    return;
                }
            }
            Err(e) => {
                fail(&format!("rules fanout: serialize crew payload: {e}"));
                return;
            }
        }
        println!(
            "  enforcement lane is PENDING: POST {payload_path} to {url}/governance/policies and \
             {url}/governance/rules, then verify via GET {url}/governance/rules/preview"
        );
    }
}

fn fail(msg: &str) {
    eprintln!("{msg}");
    std::process::exit(1);
}

/// Parse `--ambiguity-cap N` (default [`wicked_governance::DEFAULT_AMBIGUITY_CAP`]). Fail-loud on
/// junk: a typo'd cap silently defaulting would change which refs fan.
fn ambiguity_cap(args: &[String]) -> usize {
    match flag(args, "--ambiguity-cap") {
        None => wicked_governance::DEFAULT_AMBIGUITY_CAP,
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                fail(&format!(
                    "--ambiguity-cap must be a positive integer, got {v:?}"
                ));
                unreachable!("fail exits");
            }
        },
    }
}

/// `wicked-core rules relink [--db F] [--ambiguity-cap N] [--json] [--knowledge K --xedge X]` —
/// the AW-9 relink pass: re-derive rule→code `Governs` edges from qualified `symbol_ref`s at the
/// current epoch. Designed to run AFTER EVERY `wicked-estate index` (and as a crew workflow step):
/// a full re-extract re-mints code ids and prunes the old edges, and this pass is what makes the
/// links survive — durable-by-name, derived-by-id. Unresolvable refs are printed as drift and kept
/// in the report; the rule metadata is never touched. Exit 0 unless the pass itself fails —
/// residue gating is `rules drift`'s job.
fn rules_relink_cmd(args: &[String]) {
    let cap = ambiguity_cap(args);
    let resolved_db = store_path(args);
    if resolved_db.starts_with("--") {
        fail(&format!(
            "rules relink: --db has no value (resolved to {resolved_db:?})"
        ));
        return;
    }
    let mut store = match wicked_apps_core::open_store(Some(&resolved_db)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("rules relink: open store failed: {e}"));
            return;
        }
    };

    let report = match wicked_governance::relink(&mut store, cap, now_secs()) {
        Ok(r) => r,
        Err(e) => {
            fail(&format!("rules relink: {e}"));
            return;
        }
    };

    // The knowledge.relate_code half — ONLY where the seam exists (AW-9 second clause). Both
    // stores are needed (the about-edge lives in the xedge overlay, the chunk in the knowledge
    // store); a half-configured seam fails loud rather than silently skipping the other half.
    let knowledge_path = flag(args, "--knowledge").or_else(|| env_nonempty("WICKED_KNOWLEDGE_DB"));
    let xedge_path = flag(args, "--xedge").or_else(|| env_nonempty("WICKED_XEDGE_DB"));
    let knowledge_report = match (&knowledge_path, &xedge_path) {
        (Some(k), Some(x)) => {
            let engine = match wicked_estate_knowledge::KnowledgeEngine::open(k) {
                Ok(e) => e,
                Err(e) => {
                    fail(&format!("rules relink: open knowledge store {k:?}: {e}"));
                    return;
                }
            };
            let xedge = match wicked_estate_knowledge::XedgeStore::open(x) {
                Ok(s) => s,
                Err(e) => {
                    fail(&format!("rules relink: open xedge overlay {x:?}: {e}"));
                    return;
                }
            };
            match wicked_governance::relate_linked_rules(&engine, &xedge, &report.linked) {
                Ok(r) => Some(r),
                Err(e) => {
                    fail(&format!("rules relink: knowledge relate failed: {e}"));
                    return;
                }
            }
        }
        (None, None) => None,
        _ => {
            fail(
                "rules relink: the knowledge seam needs BOTH --knowledge and --xedge (or \
                 $WICKED_KNOWLEDGE_DB and $WICKED_XEDGE_DB) — refusing a half-configured seam",
            );
            return;
        }
    };

    if args.iter().any(|a| a == "--json") {
        let out = serde_json::json!({
            "relink": report,
            "knowledge": match &knowledge_report {
                Some(r) => serde_json::to_value(r).expect("report serializes"),
                None => serde_json::json!({ "skipped": "no knowledge/xedge store supplied" }),
            },
        });
        println!("{}", serde_json::to_string_pretty(&out).expect("json"));
        return;
    }

    println!(
        "rules relink: {} rule(s) linked ({} Governs edge(s) re-derived), {} with refs of {} \
         seen, {} retired skipped",
        report.linked.len(),
        report.edges_written,
        report.rules_with_ref,
        report.rules_seen,
        report.skipped_retired
    );
    for finding in &report.drift {
        // Drift is REPORTED, never dropped: the ref stays on the rule for the next pass.
        eprintln!(
            "  DRIFT {}: symbol_ref {:?} — {:?}",
            finding.rule_id, finding.symbol_ref, finding.failure
        );
    }
    match &knowledge_report {
        None => println!("  knowledge: skipped (no --knowledge/--xedge store supplied)"),
        Some(k) => {
            println!(
                "  knowledge: {} about-xedge(s) written; {} doc(s) not in the knowledge store",
                k.links_written.len(),
                k.unmatched_docs.len()
            );
            for doc in &k.unmatched_docs {
                eprintln!("    not ingested into knowledge: {doc}");
            }
        }
    }
}

/// `wicked-core rules drift [--db F] [--dir <docs>] [--ambiguity-cap N] [--json]` — the AW-10
/// drift report: the residue an idempotent on-merge re-ingest cannot self-heal. Read-only (the
/// store opens with `open_store_ro`, safe beside the live daemon). Exit codes: 0 clean, 3 residue
/// found, 1 operational error — so a CI job can gate on it directly.
fn rules_drift_cmd(args: &[String]) {
    let cap = ambiguity_cap(args);
    let resolved_db = store_path(args);
    if resolved_db.starts_with("--") {
        fail(&format!(
            "rules drift: --db has no value (resolved to {resolved_db:?})"
        ));
        return;
    }
    let docs_dir = flag(args, "--dir");
    if let Some(d) = &docs_dir {
        if d.starts_with("--") {
            fail(&format!(
                "rules drift: --dir has no value (resolved to {d:?})"
            ));
            return;
        }
    }
    let store = match wicked_apps_core::open_store_ro(Some(&resolved_db)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("rules drift: open store read-only failed: {e}"));
            return;
        }
    };

    let report = match wicked_governance::drift(
        &store,
        docs_dir.as_deref().map(std::path::Path::new),
        cap,
    ) {
        Ok(r) => r,
        Err(e) => {
            fail(&format!("rules drift: {e}"));
            return;
        }
    };

    if args.iter().any(|a| a == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("report serializes")
        );
    } else {
        println!(
            "rules drift: {} rule(s) checked, {} doc(s) scanned{}",
            report.rules_checked,
            report.docs_scanned,
            if report.docs_checked {
                ""
            } else {
                " (no --dir: doc checks skipped)"
            }
        );
        for o in &report.orphaned {
            eprintln!("  ORPHANED {} ← {} ({:?})", o.rule_id, o.doc_path, o.reason);
        }
        for u in &report.uningested {
            eprintln!(
                "  UNINGESTED {} in {} ({:?})",
                u.rule_id, u.doc_path, u.reason
            );
        }
        for f in &report.unresolvable {
            eprintln!(
                "  UNRESOLVABLE {}: symbol_ref {:?} — {:?}",
                f.rule_id, f.symbol_ref, f.failure
            );
        }
        for u in &report.unlinked {
            eprintln!(
                "  UNLINKED {}: {:?} resolves but has no Governs edge — run `rules relink`",
                u.rule_id, u.symbol_ref
            );
        }
        for x in &report.extraneous {
            eprintln!(
                "  EXTRANEOUS {} governs {} beyond its current resolution",
                x.rule_id, x.target
            );
        }
        if !report.has_residue() {
            println!("  clean — nothing to heal");
        }
    }

    // Exit 3 = residue (distinct from 1 = operational error and the gates' 2 = deny), so CI can
    // tell "the check could not run" from "the check ran and found drift".
    if report.has_residue() {
        std::process::exit(3);
    }
}

/// `wicked-core rules recall [--db F] [--language L] [--layer L] [--framework F] [--severity S]
/// [--rule-type T] [--json]` — the AW-17 recall-REPORT (arch-R15 v1): the conformance rules that
/// APPLY to the query facets, severity-ordered (critical→info, [`wicked_governance::recall_rules`]),
/// each citing its rule id + provenance ref — the `<doc path>@<blob sha>#<RULE-ID>` wiki URI a CI
/// comment links back to.
///
/// Strictly READ-ONLY (`open_store_ro`, safe beside a live single-writer daemon) and strictly a
/// REPORT: it never evaluates a diff and never exits nonzero on findings — the CI conformance
/// seam's v1 contract is recall-report, never a block. Exit 0 = report produced (an EMPTY report
/// prints a diagnostic line rather than silence, so "no rules ingested" is visible, not mistaken
/// for "conformant"); exit 1 = operational error (bad flag, unopenable store).
/// Parse the shared rule-query facets (`--language/--layer/--framework/--severity/--rule-type/
/// --type`) for `rules recall` / `rules list`. A facet flag with a flag-shaped or vocabulary-
/// violating value fails LOUD — silently treating it as a wildcard would widen the report while
/// looking targeted.
fn parse_rule_query(args: &[String], cmd: &str) -> wicked_governance::RuleQuery {
    let facet = |name: &str| -> Option<String> {
        let v = flag(args, name)?;
        if v.starts_with("--") {
            fail(&format!("{cmd}: {name} has no value (resolved to {v:?})"));
        }
        Some(v)
    };
    // Severity/rule-type parse through the SAME serde vocabulary the wire contract uses
    // (snake_case enum spellings) — a typo'd value fails loud, never defaults to "all".
    let severity: Option<wicked_governance::ConfSeverity> = facet("--severity").map(|v| {
        match serde_json::from_value(serde_json::Value::String(v.clone())) {
            Ok(s) => s,
            Err(_) => {
                fail(&format!(
                    "{cmd}: --severity must be one of info|warn|error|critical, got {v:?}"
                ));
                unreachable!("fail exits");
            }
        }
    });
    let rule_type: Option<wicked_governance::RuleType> = facet("--rule-type").map(|v| {
        match serde_json::from_value(serde_json::Value::String(v.clone())) {
            Ok(t) => t,
            Err(_) => {
                fail(&format!(
                    "{cmd}: --rule-type must be pattern or policy, got {v:?}"
                ));
                unreachable!("fail exits");
            }
        }
    });
    // The STEERING facet — validated against the same vocabulary the write boundary enforces
    // (INV-S1), so a typo'd type fails loud instead of silently matching nothing.
    let steering_type = facet("--type").inspect(|v| {
        if !wicked_governance::STEERING_TYPES.contains(&v.as_str()) {
            fail(&format!(
                "{cmd}: --type must be one of {}, got {v:?}",
                wicked_governance::STEERING_TYPES.join("|")
            ));
        }
    });
    wicked_governance::RuleQuery {
        language: facet("--language"),
        layer: facet("--layer"),
        framework: facet("--framework"),
        severity,
        rule_type,
        steering_type,
    }
}

/// One rule row of the `rules recall`/`rules list` text reports.
fn print_rule_row(r: &wicked_governance::ConformanceRule) {
    let mut markers = String::new();
    if r.retired {
        markers.push_str(" [RETIRED]");
    }
    if let Some(effect) = &r.effect {
        markers.push_str(&format!(" [effect: {effect:?}]"));
    }
    println!(
        "  {:?} {} ({}): {}{} [source: {}]",
        r.severity,
        r.id,
        r.steering_type,
        r.statement,
        markers,
        r.provenance.reference.as_deref().unwrap_or("(none)")
    );
}

fn rules_recall_cmd(args: &[String]) {
    let resolved_db = store_path(args);
    if resolved_db.starts_with("--") {
        fail(&format!(
            "rules recall: --db has no value (resolved to {resolved_db:?})"
        ));
        return;
    }
    let query = parse_rule_query(args, "rules recall");
    let store = match wicked_apps_core::open_store_ro(Some(&resolved_db)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("rules recall: open store read-only failed: {e}"));
            return;
        }
    };
    let rules = match wicked_governance::recall_rules(&store, &query) {
        Ok(rs) => rs,
        Err(e) => {
            fail(&format!("rules recall: {e}"));
            return;
        }
    };

    if args.iter().any(|a| a == "--json") {
        // One stable envelope (not a bare array): `count` lets a consumer distinguish "empty
        // report" from a parse mishap, and the echoed `query` records WHICH facets produced it.
        let report = serde_json::json!({
            "count": rules.len(),
            "query": query,
            "rules": rules,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("report serializes")
        );
    } else {
        println!(
            "rules recall: {} applicable rule(s) (store: {resolved_db})",
            rules.len()
        );
        for r in &rules {
            print_rule_row(r);
        }
        if rules.is_empty() {
            // An empty report is a DIAGNOSTIC, never silence: on a store nothing was ingested
            // into, "0 rules" must read as "nothing to recall against", not as conformance.
            println!(
                "  no rules matched — if this store was expected to hold rules, run \
                 `wicked-core rules ingest <dir> --db {resolved_db}` first"
            );
        }
    }
}

/// `wicked-core rules list [--db F] [--type T] [--include-retired] [<facets>] [--json]` — the
/// STEERING management listing over the UNIFIED steering-rule store. Where `rules recall` is the
/// enforcement funnel (active, recall-only rules — what a gate attaches), `rules list` is the
/// operator/audit view: effect-bearing (decide-lane / migrated-policy) rules always appear, and
/// `--include-retired` lists withdrawn rows too — the 0.7.5 follow-up closing the
/// recall-skips-retired listing gap. Strictly READ-ONLY (`open_store_ro`, daemon-safe).
fn rules_list_cmd(args: &[String]) {
    let resolved_db = store_path(args);
    if resolved_db.starts_with("--") {
        fail(&format!(
            "rules list: --db has no value (resolved to {resolved_db:?})"
        ));
        return;
    }
    let query = parse_rule_query(args, "rules list");
    let include_retired = args.iter().any(|a| a == "--include-retired");
    let store = match wicked_apps_core::open_store_ro(Some(&resolved_db)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("rules list: open store read-only failed: {e}"));
            return;
        }
    };
    let rules = match wicked_governance::list_rules(&store, &query, include_retired) {
        Ok(rs) => rs,
        Err(e) => {
            fail(&format!("rules list: {e}"));
            return;
        }
    };

    if args.iter().any(|a| a == "--json") {
        // Same stable envelope as recall, plus the echoed retired switch (a listing that MAY
        // contain withdrawn rows must say whether it was asked to).
        let report = serde_json::json!({
            "count": rules.len(),
            "query": query,
            "include_retired": include_retired,
            "rules": rules,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("report serializes")
        );
    } else {
        println!(
            "rules list: {} steering rule(s){} (store: {resolved_db})",
            rules.len(),
            if include_retired {
                " incl. retired"
            } else {
                ""
            }
        );
        for r in &rules {
            print_rule_row(r);
        }
        if rules.is_empty() {
            println!(
                "  no rules matched — if this store was expected to hold rules, run \
                 `wicked-core rules ingest <dir> --db {resolved_db}` first"
            );
        }
    }
}

/// `wicked-core rules scoreboard [--db F] [--dir <docs>] [--ambiguity-cap N] [--json]` — the
/// AW-23 / arch-R23 population/connection scoreboard: the report that tells a POPULATED wiki from
/// an ingested-once-and-decaying one at a glance. Typed % is doc-side (`enforcement_class` lives
/// in frontmatter, never on the rule node) so it needs the SAME `--dir` root `rules ingest` used;
/// without it that metric reports unavailable, honestly, in-band. Strictly READ-ONLY
/// (`open_store_ro`, safe beside a live single-writer daemon) and strictly a REPORT: exit 0 =
/// report produced, 1 = operational error — residue gating stays `rules drift`'s job.
fn rules_scoreboard_cmd(args: &[String]) {
    let cap = ambiguity_cap(args);
    let resolved_db = store_path(args);
    if resolved_db.starts_with("--") {
        fail(&format!(
            "rules scoreboard: --db has no value (resolved to {resolved_db:?})"
        ));
        return;
    }
    let docs_dir = flag(args, "--dir");
    // A `--dir` with no value must fail LOUD, never silently degrade to "typing unavailable" —
    // the operator asked for the doc-side metric and would read the degraded report as truth.
    if args.iter().any(|a| a == "--dir") && docs_dir.as_deref().is_none_or(|d| d.starts_with("--"))
    {
        fail(&format!(
            "rules scoreboard: --dir has no value (resolved to {docs_dir:?})"
        ));
        return;
    }
    let store = match wicked_apps_core::open_store_ro(Some(&resolved_db)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!(
                "rules scoreboard: open store read-only failed: {e}"
            ));
            return;
        }
    };

    let report = match wicked_governance::scoreboard(
        &store,
        docs_dir.as_deref().map(std::path::Path::new),
        cap,
    ) {
        Ok(r) => r,
        Err(e) => {
            fail(&format!("rules scoreboard: {e}"));
            return;
        }
    };

    if args.iter().any(|a| a == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("report serializes")
        );
        return;
    }

    // `None` = nothing to measure (0/0) — printed as such, never as 0% or 100%.
    let pct = |p: Option<f64>| p.map_or_else(|| "n/a".to_string(), |v| format!("{v:.1}%"));
    println!("rules scoreboard (store: {resolved_db})");
    println!("  population");
    println!(
        "    rules: {} active, {} retired ({} total)",
        report.rules_active, report.rules_retired, report.rules_total
    );
    // STEERING by-type breakdown — one row per steering sub-page that holds rules.
    for (steering_type, row) in &report.by_type {
        println!(
            "      {steering_type}: {} active, {} retired ({} total{})",
            row.active,
            row.retired,
            row.total,
            if row.enforcing > 0 {
                format!(", {} enforcing", row.enforcing)
            } else {
                String::new()
            }
        );
    }
    let typing = &report.typing;
    if typing.available {
        let by_class = typing
            .by_class
            .iter()
            .map(|(class, n)| format!("{class} {n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        println!(
            "    statements typed into enforcement classes: {}/{} ({}){}",
            typing.statements_typed,
            typing.statements_total,
            pct(typing.percent),
            if by_class.is_empty() {
                String::new()
            } else {
                format!("  [{by_class}]")
            }
        );
        for doc in &typing.docs_untyped {
            println!("      UNTYPED {doc} — no enforcement_class in its frontmatter");
        }
    } else {
        println!(
            "    statements typed: unavailable — {}",
            typing.reason.as_deref().unwrap_or("(no reason recorded)")
        );
    }
    println!("  connection");
    println!(
        "    symbol_refs resolving at current epoch: {}/{} ({}){}",
        report.connection.refs_resolving,
        report.connection.rules_with_ref,
        pct(report.connection.percent),
        if report.connection.refs_unresolvable > 0 {
            format!(
                "  ({} unresolvable — `rules drift` names them)",
                report.connection.refs_unresolvable
            )
        } else {
            String::new()
        }
    );
    println!(
        "    rules with live Governs links: {}",
        report.connection.rules_linked
    );
    println!("  enforcement evidence");
    println!(
        "    denials citing wiki rules: {} distinct deny claim(s), {} evidenced_by edge(s)",
        report.evidence.denial_claims, report.evidence.evidenced_by_edges
    );
    println!(
        "    rules cited by at least one denial: {}",
        report.evidence.rules_evidenced
    );
    println!(
        "    Governs evidence_count total: {}",
        report.evidence.governs_evidence_total
    );
    for row in &report.evidence.per_rule {
        println!(
            "      {}: {} denial claim(s), governs evidence {}",
            row.rule_id, row.denial_claims, row.governs_evidence
        );
    }
    println!(
        "  recall volume: unavailable — {}",
        report.recall_volume.reason
    );
}

/// `wicked-core rules retire (--id ID)... [--doc PATH]... --manifest M.json [--out receipt.json]`
/// — the AW-24 / arch-R22 bad-rule kill switch. OPERATOR-ONLY (arch-R22 item 1 — no agent
/// self-retirement; a worker that dislikes a rule argues in the run transcript, not the rule
/// store): one manifest-keyed operation retires the enforcement copy, every discovery-graph copy,
/// and marks the knowledge rationale non-normative, then re-opens each cli lane FRESH and verifies
/// the withdrawn state through the consumer read path. `--doc` retires every rule the manifest
/// derived from that doc path — the deleted-doc → explicit-retire bridge (`rules drift` reports
/// the orphans; this clears them; drift then counts them as `skipped_retired`, the healed state).
/// `wicked-core rules eval …` — the governance eval lane. Two modes:
/// - default: replay a behavior corpus through the REAL SELECT→DECIDE gate path against the rules
///   in `--db` and score caught/gap/false-positive per sample. READ-ONLY (`open_store_ro`,
///   daemon-safe) and strictly a report: exit 0 = report produced (gaps included), 1 = operational
///   error — the eval measures the ruleset, it does not gate on it.
/// - `--import <name> [<dir>]`: ingest a corpus into the estate KNOWLEDGE store under
///   `evals:<name>` (id-keyed, WITH embeddings) — the one write path, and it never touches the
///   rules store.
fn rules_eval_cmd(args: &[String]) {
    // The value-taking flags this subcommand knows; the import <dir> is the first non-flag token
    // after `rules eval` (index 3+) — the same walk `rules ingest` uses, so `--import x /dir` and
    // `/dir --import x` both resolve.
    const VALUE_FLAGS: &[&str] = &["--db", "--knowledge-db", "--corpus", "--type", "--import"];
    let positional = || -> Option<String> {
        let mut i = 3;
        while i < args.len() {
            let a = &args[i];
            if a.starts_with("--") {
                let has_value = VALUE_FLAGS.contains(&a.as_str())
                    && args.get(i + 1).is_some_and(|v| !v.starts_with("--"));
                i += if has_value { 2 } else { 1 };
            } else {
                return Some(a.clone());
            }
        }
        None
    };
    // Flag-shaped-value guard (same posture as ingest/recall): a missing value must fail loud,
    // never be read as the next flag.
    let guarded = |name: &str| -> Option<String> {
        let v = flag(args, name)?;
        if v.starts_with("--") {
            fail(&format!(
                "rules eval: {name} has no value (resolved to {v:?})"
            ));
        }
        Some(v)
    };

    let knowledge_db =
        guarded("--knowledge-db").unwrap_or_else(wicked_governance::default_knowledge_db);

    // ── import mode: corpus → estate knowledge store, receipt out ──
    if let Some(name) = guarded("--import") {
        let source = match positional() {
            Some(dir) => wicked_governance::CorpusSource::Dir(std::path::PathBuf::from(dir)),
            None => wicked_governance::CorpusSource::Builtin,
        };
        let samples = match wicked_governance::load_corpus(&source, Some(&knowledge_db)) {
            Ok(s) => s,
            Err(e) => {
                fail(&format!("rules eval --import: {e}"));
                return;
            }
        };
        match wicked_governance::import_corpus(&knowledge_db, &name, &samples, now_secs()) {
            Ok(receipt) => println!(
                "{}",
                serde_json::to_string_pretty(&receipt).expect("receipt serializes")
            ),
            Err(e) => fail(&format!("rules eval --import: {e}")),
        }
        return;
    }

    // ── eval mode ──
    let resolved_db = store_path(args);
    if resolved_db.starts_with("--") {
        fail(&format!(
            "rules eval: --db has no value (resolved to {resolved_db:?})"
        ));
        return;
    }
    let source = match guarded("--corpus") {
        None => wicked_governance::CorpusSource::Builtin,
        Some(c) if c.starts_with(wicked_governance::EVAL_SCOPE_PREFIX) => {
            wicked_governance::CorpusSource::Scope(c)
        }
        Some(c) => {
            let p = std::path::PathBuf::from(&c);
            if p.is_dir() {
                wicked_governance::CorpusSource::Dir(p)
            } else {
                fail(&format!(
                    "rules eval: --corpus {c:?} is neither an evals:<name> knowledge scope nor an \
                     existing directory of sample *.json files"
                ));
                return;
            }
        }
    };
    let steering_type = guarded("--type");
    let samples = match wicked_governance::load_corpus(&source, Some(&knowledge_db)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("rules eval: {e}"));
            return;
        }
    };
    let store = match wicked_apps_core::open_store_ro(Some(&resolved_db)) {
        Ok(s) => s,
        Err(e) => {
            fail(&format!("rules eval: open store read-only failed: {e}"));
            return;
        }
    };
    let report = match wicked_governance::run_evals(
        &store,
        &samples,
        steering_type.as_deref(),
        Some(&knowledge_db),
        now_secs(),
    ) {
        Ok(r) => r,
        Err(e) => {
            fail(&format!("rules eval: {e}"));
            return;
        }
    };

    if args.iter().any(|a| a == "--json") {
        // The serde output IS the wire contract — printed verbatim (pretty for terminals).
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("report serializes")
        );
        return;
    }
    let s = &report.summary;
    let degraded = match &report.degraded {
        Some(d) => format!(" [hints degraded: {d}]"),
        None => String::new(),
    };
    println!(
        "rules eval: {} sample(s) — {} caught, {} gap(s), {} false positive(s){degraded} \
         (rules store: {resolved_db})",
        s.total, s.caught, s.gaps, s.false_positives
    );
    for r in &report.results {
        let tag = match r.verdict {
            wicked_governance::Verdict::Caught => "CAUGHT",
            wicked_governance::Verdict::Gap => "GAP   ",
            wicked_governance::Verdict::FalsePositive => "FALSE+",
        };
        let mut line = format!(
            "  {tag}  {} ({}, {})",
            r.sample.id,
            r.sample.steering_type,
            match r.sample.kind {
                wicked_governance::SampleKind::Good => "good",
                wicked_governance::SampleKind::Bad => "bad",
            }
        );
        if !r.fired.is_empty() {
            line.push_str(&format!(" — fired: {}", r.fired.join(", ")));
        }
        if let Some(nearest) = &r.nearest_rules {
            if nearest.is_empty() {
                line.push_str(" — no nearby rules");
            } else {
                let hints: Vec<String> = nearest
                    .iter()
                    .map(|n| format!("{} ({:.3})", n.rule_id, n.similarity))
                    .collect();
                line.push_str(&format!(" — nearest: {}", hints.join(", ")));
            }
        }
        println!("{line}");
    }
    if s.total == 0 {
        println!("  no samples matched — check --type / --corpus");
    }
}

fn rules_retire_cmd(args: &[String]) {
    const VALUE_FLAGS: &[&str] = &["--id", "--doc", "--manifest", "--out"];
    for name in VALUE_FLAGS {
        for v in flag_all(args, name) {
            if v.starts_with("--") {
                fail(&format!(
                    "rules retire: {name} has no value (resolved to {v:?}) — refusing a \
                     flag-shaped argument"
                ));
                return;
            }
        }
    }

    let manifest_path = match flag(args, "--manifest") {
        Some(m) => m,
        None => {
            fail(
                "rules retire requires the fan-out manifest the rule was imported with: \
                 wicked-core rules retire (--id <RULE-ID>)... [--doc <PATH>]... --manifest \
                 <M.json> [--out <receipt.json>] — retirement is manifest-keyed (arch-R22) so ONE \
                 op reaches every lane copy",
            );
            return;
        }
    };
    let manifest_text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => {
            fail(&format!("rules retire: cannot read {manifest_path}: {e}"));
            return;
        }
    };
    let manifest: wicked_governance::FanoutManifest = match serde_json::from_str(&manifest_text) {
        Ok(m) => m,
        Err(e) => {
            fail(&format!(
                "rules retire: {manifest_path} is not a fan-out manifest: {e}"
            ));
            return;
        }
    };

    // The retire set: explicit --id values, plus every rule the manifest derived from each --doc.
    // A --doc matching NOTHING fails loud — "I retired the deleted doc's rules" must never be
    // claimable when zero rules were actually selected (the silent-orphan shape in reverse).
    let mut ids = flag_all(args, "--id");
    for doc in flag_all(args, "--doc") {
        let derived = wicked_governance::select_doc_rules(&manifest, &doc);
        if derived.is_empty() {
            fail(&format!(
                "rules retire: --doc {doc:?} matches no rule source in {manifest_path} — check \
                 the path against the manifest rows' `source` refs (path component, no @sha / \
                 #anchor). No store was modified."
            ));
            return;
        }
        println!("rules retire: --doc {doc} → {}", derived.join(", "));
        ids.extend(derived);
    }
    if ids.is_empty() {
        fail("rules retire: nothing to retire — pass --id <RULE-ID> and/or --doc <PATH>");
        return;
    }

    // NEVER the CLI against a daemon-held store — same fence as `rules fanout`, applied to every
    // cli target the SELECTED manifest rows would write. A daemon-held enforcement lane already
    // travels as transport crew-api (recorded pending); a fenced discovery/knowledge path means
    // the manifest itself was authored against the wrong store.
    let mut fenced_targets: Vec<String> = Vec::new();
    if manifest.enforcement.transport == "cli" {
        fenced_targets.push(manifest.enforcement.target.clone());
    }
    for id in &ids {
        if let Some(entry) = manifest.rules.get(id) {
            fenced_targets.extend(entry.discovery.iter().cloned());
            fenced_targets.extend(entry.knowledge.iter().map(|k| {
                k.split_once("#kchunk:")
                    .map_or(k.as_str(), |(db, _)| db)
                    .to_string()
            }));
        }
    }
    for path in &fenced_targets {
        if let Some(fence) = daemon_fenced(path) {
            fail(&format!(
                "rules retire: {path:?} resolves under the crew daemon's state home ({fence}) — a \
                 daemon-held store is never CLI-written (single-writer invariant); retire that \
                 lane over DELETE /api/v1/governance/rules/<id> instead"
            ));
            return;
        }
    }

    let receipt = match wicked_governance::retire_from_manifest(
        &manifest,
        &manifest_path,
        &ids,
        now_secs(),
    ) {
        Ok(r) => r,
        Err(e) => {
            fail(&format!("rules retire: {e}"));
            return;
        }
    };

    println!(
        "rules retire: {} id(s) across the manifest's lanes",
        receipt.requested.len()
    );
    for retirement in &receipt.retirements {
        println!("  {} ({})", retirement.id, retirement.kind);
        for lane in &retirement.lanes {
            let status = match lane.status {
                wicked_governance::LaneStatus::Retired => "RETIRED",
                wicked_governance::LaneStatus::AlreadyRetired => "ALREADY-RETIRED",
                wicked_governance::LaneStatus::Absent => "ABSENT (audit note in receipt)",
                wicked_governance::LaneStatus::Pending => "PENDING (crew API)",
                wicked_governance::LaneStatus::Failed => "FAILED",
            };
            println!(
                "    {:<11} [{}] {} — {}{}",
                lane.lane,
                lane.transport,
                lane.target,
                status,
                if lane.verified { ", verified" } else { "" }
            );
            if lane.status == wicked_governance::LaneStatus::Failed
                || lane.status == wicked_governance::LaneStatus::Pending
            {
                if let Some(note) = &lane.note {
                    println!("      {note}");
                }
            }
        }
    }

    let receipt_path = flag(args, "--out").unwrap_or_else(|| "retire-receipt.json".into());
    let json = match serde_json::to_string_pretty(&receipt) {
        Ok(j) => j,
        Err(e) => {
            fail(&format!("rules retire: serialize receipt: {e}"));
            return;
        }
    };
    if let Err(e) = std::fs::write(&receipt_path, json) {
        fail(&format!("rules retire: cannot write {receipt_path}: {e}"));
        return;
    }
    println!("  receipt → {receipt_path}");

    if receipt.pending > 0 {
        println!(
            "  {} lane(s) PENDING: complete the crew-api DELETE(s) above, then verify via \
             GET <crew>/governance/rules/preview — the kill switch is NOT fully propagated until \
             then",
            receipt.pending
        );
    }
    if !receipt.all_cli_lanes_verified {
        fail(
            "rules retire: one or more lanes FAILED verification (see receipt) — the rule may \
             still be recallable somewhere a governed run reads; re-run after fixing the named \
             lane, and treat the estate as UNGOVERNED for this rule until the receipt verifies",
        );
    }
}
