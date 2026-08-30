#!/usr/bin/env python3
"""AW-13 seed runbook driver — doctrine corpus → scratch stores → recall proof.

Drives the shipped machinery end to end, exactly as `seed/README.md` documents:

  1. STAGE    seed/corpus/*.md + <estate>/docs/adr/*.md into ONE staging root
              (single ingest/drift root — per-root drift over a multi-root corpus
              would false-positive every other root's rules as orphans).
  2. INDEX    the estate repo into the scratch discovery graph
              (`wicked-estate index`) — the store `rules relink` resolves
              symbol_refs against, and the store the worker's MCP binds.
  3. FANOUT   `wicked-core rules fanout <staging> --scope workspace` into the
              enforcement / discovery / knowledge scratch stores; the manifest
              is the receipt (rules + policies + rule_sets).
  4. RELINK   `wicked-core rules relink` — derive the Governs edges for the
              engine-contract symbol_refs (the doc<->gate pairing, AW-9), plus
              knowledge relate via the xedge overlay.
  5. DRIFT    `wicked-core rules drift` — must be clean (exit 0) right after a
              seed: anything else is residue the seed itself introduced.
  6. KNOWLEDGE bulk docs as knowledge (`knowledge.ingest` over MCP stdio,
              scope wiki:<area>, stable source URIs) — manifest item 7.
  7. PROVE    against the INSTALLED released `wicked-estate-mcp` binary:
              tools/list (rules.recall present, knowledge.recall has
              scope_prefix), rules.recall (citations = provenance refs),
              RulesInventory (RuleSets populated), knowledge.recall with
              {scope_prefix: "wiki:"} (cited sources). Responses land in
              --evidence as JSON.

Scratch-only: every store lives under --scratch; the script never touches
~/.wicked-estate, ~/.wicked-crew, ~/.wicked-brain, or any daemon-held store.

Python 3 stdlib only; no shell, forward-slash paths in every identifier
(cross-platform mandate).
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent  # .../crates/wicked-governance/seed


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    print(f"$ {' '.join(str(c) for c in cmd)}", flush=True)
    return subprocess.run([str(c) for c in cmd], **kw)


def run_checked(cmd: list[str], log: Path | None = None, env: dict[str, str] | None = None) -> str:
    proc = run(cmd, capture_output=True, text=True, env=env)
    out = (proc.stdout or "") + (proc.stderr or "")
    if log:
        log.write_text(out, encoding="utf-8")
    if proc.returncode != 0:
        sys.exit(f"FATAL: {' '.join(str(c) for c in cmd)} exited {proc.returncode}\n{out}")
    return proc.stdout or ""


# ── MCP stdio client (newline-delimited JSON-RPC 2.0) ────────────────────────


class McpClient:
    def __init__(self, binary: Path, env: dict[str, str]):
        self.proc = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
        self._id = 0
        self.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "aw13-seed-runbook", "version": "1.0"},
            },
        )
        self.notify("notifications/initialized", {})

    def _send(self, obj: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def notify(self, method: str, params: dict) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method: str, params: dict) -> dict:
        self._id += 1
        self._send({"jsonrpc": "2.0", "id": self._id, "method": method, "params": params})
        assert self.proc.stdout is not None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                err = self.proc.stderr.read() if self.proc.stderr else ""
                sys.exit(f"FATAL: MCP server closed stdout during {method}\n{err}")
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            if msg.get("id") == self._id:
                if "error" in msg:
                    sys.exit(f"FATAL: MCP {method} error: {json.dumps(msg['error'])}")
                return msg["result"]
            # server-initiated notification/log — skip

    def tool(self, name: str, arguments: dict) -> dict:
        return self.request("tools/call", {"name": name, "arguments": arguments})

    def close(self) -> None:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
            self.proc.terminate()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def tool_text(result: dict) -> dict | list | str:
    """The JSON payload inside a tools/call result's first text content block."""
    content = result.get("content", [])
    for block in content:
        if block.get("type") == "text":
            try:
                return json.loads(block["text"])
            except (json.JSONDecodeError, KeyError):
                return block.get("text", "")
    return result


# ── bulk-doc chunking (manifest item 7) ──────────────────────────────────────


def chunk_doc(text: str, max_chunk: int = 3500, max_chunks: int = 40) -> list[str]:
    """Split on `## ` headings, then hard-wrap oversized sections. Deterministic."""
    sections: list[str] = []
    current: list[str] = []
    for line in text.splitlines():
        if line.startswith("## ") and current:
            sections.append("\n".join(current))
            current = [line]
        else:
            current.append(line)
    if current:
        sections.append("\n".join(current))
    chunks: list[str] = []
    for sec in sections:
        sec = sec.strip()
        while sec:
            chunks.append(sec[:max_chunk])
            sec = sec[max_chunk:]
    return [c for c in chunks if c][:max_chunks]


# ── the runbook ──────────────────────────────────────────────────────────────


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--core-bin", required=True, help="wicked-core binary (rules ingest/fanout/relink/drift)")
    ap.add_argument("--estate-bin", required=True, help="installed wicked-estate binary (index)")
    ap.add_argument("--mcp-bin", required=True, help="installed wicked-estate-mcp binary (the recall proof target)")
    ap.add_argument("--estate-src", required=True, help="wicked-estate checkout to index + stage docs/adr from")
    ap.add_argument("--workspace", required=True, help="wicked workspace root (scratch/TARGET-ARCHITECTURE.md, CLAUDE.md)")
    ap.add_argument("--bus-spec", required=True, help="path to wicked-bus reqs/SPEC.md (bulk-knowledge source)")
    ap.add_argument("--scratch", required=True, help="work dir for staging + ALL scratch stores (never a real store)")
    ap.add_argument("--evidence", required=True, help="output dir for the committed evidence captures")
    args = ap.parse_args()

    scratch = Path(args.scratch).resolve()
    evidence = Path(args.evidence).resolve()
    estate_src = Path(args.estate_src).resolve()
    workspace = Path(args.workspace).resolve()
    scratch.mkdir(parents=True, exist_ok=True)
    evidence.mkdir(parents=True, exist_ok=True)

    stores = {
        "enforcement": scratch / "enforcement.db",
        "discovery": scratch / "discovery-graph.db",
        "knowledge": scratch / "knowledge.db",
        "memory": scratch / "memory.db",
        "xedge": scratch / "xedge.db",
        "events": scratch / "events.db",
    }
    # Keep the emit seam scratch-contained too: lifecycle events (wicked.estate.rule.ingested)
    # land in a dedicated scratch events store, and any dead-letter spool stays under --scratch —
    # never the user's home outbox.
    core_env = dict(os.environ)
    core_env.update(
        WICKED_ESTATE_DB=str(scratch / "events.db"),
        WICKED_APPS_EMIT_DEADLETTER=str(scratch / "emit-outbox.ndjson"),
    )
    for guard in (Path.home() / ".wicked-estate", Path.home() / ".wicked-crew", Path.home() / ".wicked-brain"):
        for p in stores.values():
            if guard in p.parents:
                sys.exit(f"FATAL: scratch store {p} resolves under {guard} — refusing to touch real state")

    # 1. STAGE — one ingest root: seed/corpus/** + <estate>/docs/adr/*.md under adr/.
    staging = scratch / "staging"
    if staging.exists():
        shutil.rmtree(staging)
    shutil.copytree(HERE / "corpus", staging)
    adr_src = estate_src / "docs" / "adr"
    shutil.copytree(adr_src, staging / "adr")
    estate_rev = run_checked(["git", "-C", estate_src, "rev-parse", "HEAD"]).strip()
    print(f"staged: corpus + {len(list((staging / 'adr').glob('*.md')))} ADRs (estate @ {estate_rev})")

    # 2. INDEX the estate repo into the scratch discovery graph.
    run_checked(
        [args.estate_bin, "index", estate_src, "--db", stores["discovery"]],
        log=scratch / "index.log",
    )

    # 3. FANOUT across the deliberate store split (workspace scope: the proof run
    #    uses the one indexed repo graph as the discovery stand-in — production
    #    seeding enumerates EVERY live repo graph, manifest assumption 2).
    fanout_manifest = scratch / "fanout-manifest.json"
    run_checked(
        [
            args.core_bin, "rules", "fanout", staging,
            "--scope", "workspace",
            "--enforcement-db", stores["enforcement"],
            "--discovery-db", stores["discovery"],
            "--knowledge-db", stores["knowledge"],
            "--knowledge-scope", "wiki:architecture",
            "--manifest", fanout_manifest,
        ],
        log=scratch / "fanout.log",
        env=core_env,
    )
    shutil.copy2(fanout_manifest, evidence / "fanout-manifest.json")

    # 4. RELINK — derive rule→code Governs edges from the engine-contract
    #    symbol_refs at the current epoch + relate knowledge via the xedge seam.
    relink_out = run_checked(
        [
            args.core_bin, "rules", "relink",
            "--db", stores["discovery"],
            "--knowledge", stores["knowledge"],
            "--xedge", stores["xedge"],
            "--json",
        ],
        log=scratch / "relink.log",
        env=core_env,
    )
    (evidence / "relink-report.json").write_text(relink_out, encoding="utf-8")
    relink = json.loads(relink_out)
    drift_findings = relink.get("relink", {}).get("drift", [])
    if drift_findings:
        sys.exit(f"FATAL: relink reported drift findings on a fresh seed: {drift_findings}")
    if not relink.get("relink", {}).get("linked"):
        sys.exit("FATAL: relink linked nothing — the engine-contract symbol_refs did not resolve")

    # 5. DRIFT — read-only residue check over the SAME root the ingest used.
    drift = run(
        [str(args.core_bin), "rules", "drift", "--dir", str(staging), "--db", str(stores["discovery"]), "--json"],
        capture_output=True, text=True, env=core_env,
    )
    (evidence / "drift-report.json").write_text(drift.stdout or drift.stderr, encoding="utf-8")
    if drift.returncode != 0:
        sys.exit(f"FATAL: rules drift exited {drift.returncode} right after the seed:\n{drift.stdout}\n{drift.stderr}")

    # 6+7. KNOWLEDGE bulk ingest + the recall proof, over the installed MCP binary.
    env = dict(os.environ)
    env.update(
        WICKED_ESTATE_DB=str(stores["discovery"]),
        WICKED_KNOWLEDGE_DB=str(stores["knowledge"]),
        WICKED_MEMORY_DB=str(stores["memory"]),
        WICKED_XEDGE_DB=str(stores["xedge"]),
    )
    client = McpClient(Path(args.mcp_bin), env)
    try:
        tools = client.request("tools/list", {})
        (evidence / "tool-list.json").write_text(json.dumps(tools, indent=2), encoding="utf-8")
        names = {t["name"]: t for t in tools.get("tools", [])}
        if "rules.recall" not in names:
            sys.exit("FATAL: installed wicked-estate-mcp does not advertise rules.recall (XC-3 regression)")
        kr = names.get("knowledge.recall", {})
        if "scope_prefix" not in json.dumps(kr.get("inputSchema", {})):
            sys.exit("FATAL: knowledge.recall has no scope_prefix in the installed binary (XC-3 regression)")

        # 6. Bulk docs as knowledge (manifest item 7): scope wiki:<area>, stable source URIs.
        bulk = [
            ("TARGET-ARCHITECTURE (four-plane model)", workspace / "scratch" / "TARGET-ARCHITECTURE.md",
             "wiki:architecture", "workspace://scratch/TARGET-ARCHITECTURE.md"),
            ("Root CLAUDE.md (wicked-* ecosystem)", workspace / "CLAUDE.md",
             "wiki:architecture", "workspace://CLAUDE.md"),
            ("Engine Contract", estate_src / "docs" / "ENGINE-CONTRACT.md",
             "wiki:architecture", "wicked-estate://docs/ENGINE-CONTRACT.md"),
            ("Agent-Behavior Rules R1-R7", estate_src / "docs" / "agent-behavior-rules.md",
             "wiki:architecture", "wicked-estate://docs/agent-behavior-rules.md"),
            ("wicked-bus SPEC (event grammar + catalog)", Path(args.bus_spec),
             "wiki:events", "wicked-bus://reqs/SPEC.md"),
        ]
        for adr in sorted((estate_src / "docs" / "adr").glob("*.md")):
            bulk.append((adr.stem, adr, "wiki:adr", f"wicked-estate://docs/adr/{adr.name}"))
        ingest_receipts = []
        for title, path, scope, source in bulk:
            chunks = chunk_doc(path.read_text(encoding="utf-8"))
            result = client.tool(
                "knowledge.ingest",
                {"title": title, "chunks": chunks, "scope": scope, "source": source},
            )
            ingest_receipts.append({"title": title, "scope": scope, "source": source,
                                    "chunks": len(chunks), "result": tool_text(result)})
        (evidence / "knowledge-ingest-receipts.json").write_text(
            json.dumps(ingest_receipts, indent=2), encoding="utf-8")

        # 7. The recall ACs, captured verbatim.
        captures = {
            "rules-recall-all.json": client.tool("rules.recall", {}),
            "rules-recall-critical.json": client.tool("rules.recall", {"severity": "critical"}),
            "rules-inventory.json": client.tool("RulesInventory", {}),
            "knowledge-recall-wiki-storage.json": client.tool(
                "knowledge.recall",
                {"query": "single-writer SQLite embedded store doctrine", "scope_prefix": "wiki:"}),
            "knowledge-recall-wiki-events.json": client.tool(
                "knowledge.recall",
                {"query": "event naming convention four segments producer domain", "scope_prefix": "wiki:"}),
            "knowledge-recall-wiki-planes.json": client.tool(
                "knowledge.recall",
                {"query": "cross-plane contract experience control capability foundation", "scope_prefix": "wiki:"}),
        }
        for fname, result in captures.items():
            (evidence / fname).write_text(json.dumps(result, indent=2), encoding="utf-8")

        # Hard assertions — the ACs, not vibes.
        rules_all = tool_text(captures["rules-recall-all.json"])
        got_ids = {r["id"] for r in rules_all["rules"]}
        expected = {
            "POL-1301", "PAT-1302", "PAT-1303", "PAT-1304", "PAT-1305", "PAT-1306", "PAT-1307", "PAT-1308",
            "POL-1401", "PAT-1402", "PAT-1403", "POL-1501", "PAT-1502",
            "PAT-1601", "PAT-1602", "PAT-1603", "PAT-1604", "PAT-1605", "PAT-1606", "PAT-1607",
            "PAT-1701", "PAT-1702", "PAT-1703", "POL-1704", "POL-1801", "POL-1802",
            "PAT-1901", "PAT-1902", "PAT-1903", "PAT-1904",
            "POL-2001", "POL-2002", "PAT-2003", "PAT-2004", "PAT-2005", "PAT-2006",
            "POL-1101", "PAT-1102", "PAT-1103", "POL-1104",
        }
        missing = expected - got_ids
        if missing:
            sys.exit(f"FATAL: rules.recall is missing seed rules: {sorted(missing)}")
        uncited = [r["id"] for r in rules_all["rules"]
                   if r["id"] in expected and not r.get("provenance", {}).get("ref")]
        if uncited:
            sys.exit(f"FATAL: recalled rules without a provenance ref (citation): {uncited}")
        for fname in ("knowledge-recall-wiki-storage.json", "knowledge-recall-wiki-events.json",
                      "knowledge-recall-wiki-planes.json"):
            payload = tool_text(captures[fname])
            items = payload.get("items") or payload.get("results") or []
            if not items:
                sys.exit(f"FATAL: {fname}: knowledge.recall with scope_prefix 'wiki:' returned nothing")
            unsourced = [i for i in items if not i.get("source")]
            if unsourced:
                sys.exit(f"FATAL: {fname}: recalled knowledge without a source citation: {unsourced[:2]}")
        inventory = tool_text(captures["rules-inventory.json"])
        summary = {
            "date": time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime()),
            "estate_rev": estate_rev,
            "binaries": {
                "wicked-core": str(args.core_bin),
                "wicked-estate": str(args.estate_bin),
                "wicked-estate-mcp": str(args.mcp_bin),
                "note": "released versions recorded in versions.txt beside this file",
            },
            "stores": {k: str(v) for k, v in stores.items()},
            "rules_recalled": len(rules_all["rules"]),
            "seed_rule_ids_verified": sorted(expected),
            "inventory": inventory,
        }
        (evidence / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    finally:
        client.close()

    print("\nAW-13 seed proof complete — evidence in", evidence)


if __name__ == "__main__":
    main()
