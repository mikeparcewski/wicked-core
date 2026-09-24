#!/usr/bin/env python3
"""Regression tests for gen_event_catalog.py (stdlib unittest; run from anywhere):

    python3 crates/wicked-governance/seed/test_gen_event_catalog.py

1. A mid-file `#[cfg(test)]` item must not hide the emit seams below it. The old
   `strip_test_tail` cut each file at its first top-level `#[cfg(test)]`, so
   src/cli_runner.rs's `#[cfg(test)]` helper at :771 dropped the TASK_DISPATCHED /
   TASK_COMPLETED emits at :1423 / :2115 from EVENTS.md.
2. `wicked.team.*` (DES-TEAMING-002 §6) is a whitelisted producer domain.
"""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("gen_event_catalog", HERE / "gen_event_catalog.py")
gen = importlib.util.module_from_spec(_spec)
sys.dont_write_bytecode = True
_spec.loader.exec_module(gen)


class EmitWiringSurvivesMidFileTestItems(unittest.TestCase):
    SRC = "\n".join([
        'pub const EV_A: &str = "wicked.crew.a.done";',          # line 1
        'pub const EV_B: &str = "wicked.crew.b.done";',          # line 2
        "#[cfg(test)]",
        "fn helper() -> &'static str { \"}\" }",
        "fn real() {",
        "    let ev = BusEmit::new(EV_A, CORE_DOMAIN, \"x\", p);",  # a production emit AFTER a test item
        "}",
        "#[cfg(test)]",
        "mod tests {",
        "    fn t() { let e = BusEmit::new(EV_B, D, \"x\", p); }",  # test-only emit: not a seam
        "}",
    ]) + "\n"

    def wiring(self):
        with tempfile.TemporaryDirectory() as ws:
            src = Path(ws) / "wicked-core" / "src"
            src.mkdir(parents=True)
            (src / "x.rs").write_text(self.SRC, encoding="utf-8")
            (Path(ws) / "wicked-core" / "crates").mkdir()
            consts = {
                "EV_A": {"type": "wicked.crew.a.done", "decl_file": "src/x.rs", "line": 1},
                "EV_B": {"type": "wicked.crew.b.done", "decl_file": "src/x.rs", "line": 2},
            }
            return gen.scan_core_emit_wiring(Path(ws), consts)

    def test_an_emit_after_a_mid_file_test_item_is_wired(self):
        self.assertEqual(self.wiring()["EV_A"], ["src/x.rs"])

    def test_an_emit_inside_a_test_module_is_not_wired(self):
        self.assertEqual(self.wiring()["EV_B"], [])


class TeamIsAWhitelistedDomain(unittest.TestCase):
    def test_team_events_have_no_grammar_violation(self):
        events = {"wicked.team.path.started": {}, "wicked.team.finding.raised": {}}
        self.assertEqual(gen.grammar_violations(events), [])


if __name__ == "__main__":
    unittest.main()
