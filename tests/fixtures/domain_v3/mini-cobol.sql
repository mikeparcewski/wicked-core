-- J6 C2 real-shape fixture: the estate store after garden's domain-run wrote one run (DES-C2-v2 F27, R23).
-- Writer: garden 2b4066d71410b5c08d46971de0a378cfb0824d76 scripts/domain/store.py rows_for (:303-381) through wicked-estate 0.35.10 write-batch.
-- Generator: tests/fixtures/domain_v3/gen/gen.py sha256 3e3a9e0b32ea0e80a35ed22ef1c91e592d612efb7069da54799b3c1b6db883f3; seat answers are scenario fakes.
-- tree app/cbl/PAYCALC1.cbl sha256 267e27f59e247d816abe90638c8da5d8672f5acaa3e0afebff90f3a39d63093a
-- tree app/cpy/PAYREC.cpy sha256 669409547ef2ced5076527ad6ca4655ded279311ccad593a7f04e071151e9e75
-- tree app/jcl/PAYJOB.jcl sha256 97db6c568bff71696d2fdaacb0bc8dfdc4fa0360848ce3e69465dbf61fb80401
-- Regenerate: python3 <generator> <garden checkout> <scratch dir> <this file>; meta.indexed_root is neutralized.
BEGIN TRANSACTION;
CREATE TABLE access_log (
  item_id     TEXT    NOT NULL,
  session_id  TEXT    NOT NULL,
  accessed_at INTEGER NOT NULL   -- epoch millis (matches the brain source clock)
);
CREATE TABLE annotations (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  node_sym          INTEGER NOT NULL,
  key               TEXT NOT NULL,
  value             TEXT NOT NULL,
  confidence        REAL    NOT NULL DEFAULT 1.0,
  provenance        TEXT    NOT NULL DEFAULT '',
  author            TEXT    NOT NULL DEFAULT '',
  ts                INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
  type              TEXT    NOT NULL DEFAULT 'note',
  source_type       TEXT    NOT NULL DEFAULT 'unspecified',
  extraction_method TEXT    NOT NULL DEFAULT 'manual',
  last_verified     INTEGER NOT NULL DEFAULT 0
);
INSERT INTO "annotations" VALUES(1,6,'business_rule:ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#0','{"batch_hash": "c459054eb9df4c999782c61f54c4e791066b21442332cc71b23f67b5beb691f7", "content_hash": "282ce2b251039116792473aabb9ef47d13aaf3e9a231374a2985cdb622440de6", "contract": {"anchor": {"file": "app/cbl/PAYCALC1.cbl", "lines": [20, 29], "symbol_id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA()."}, "call": "PAYCALC1.cbl-c0", "check": {"claims": [{"both": false, "from_pass": "a", "i": 0, "verdict": "SUPPORTED"}, {"both": false, "from_pass": "a", "i": 1, "verdict": "SUPPORTED"}, {"both": false, "from_pass": "a", "i": 2, "verdict": "SUPPORTED"}], "n": "1"}, "claims": [{"i": 0, "lines": ["app/cbl/PAYCALC1.cbl:24"], "text": "`PAY-HOURS > 40`", "type": "condition"}, {"i": 1, "lines": ["app/cbl/PAYCALC1.cbl:25-26"], "text": "`PAY-AMOUNT` gets the overtime rate", "type": "effect", "when": [0]}, {"i": 2, "lines": ["app/cbl/PAYCALC1.cbl:27-28"], "text": "`PAY-AMOUNT` is `PAY-RATE * PAY-HOURS`", "type": "else"}], "fetched": [], "fetched_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy"], "id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#0", "kind": "rule", "m_fail": [], "name": "READ-PARA", "r": null, "sentence": null, "status": "Checked", "status1": "Checked", "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA()."}, "fetched_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy"], "statement": "Runs: READ-PARA (app/cbl/PAYCALC1.cbl:20-29)\n- When `PAY-HOURS > 40`: `PAY-AMOUNT` gets the overtime rate [25-26]\n- Always: `PAY-AMOUNT` is `PAY-RATE * PAY-HOURS` [27-28]", "status": "Checked", "target_hash": "9b5154c88c14cdffb0266ff6c47e6882a838d3304d01ff9ee4e8e614a9e4b6d7"}',1.0,'','wicked-garden/domain-run',1795586989,'business_rule','unspecified','manual',0);
INSERT INTO "annotations" VALUES(2,6,'business_rule:ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#1','{"batch_hash": "c459054eb9df4c999782c61f54c4e791066b21442332cc71b23f67b5beb691f7", "content_hash": "b1f1c7fe019781d2d4130ef16adf5340492e9b48e792f01dee0e60d1c6844401", "contract": {"anchor": {"file": "app/cbl/PAYCALC1.cbl", "lines": [20, 29], "symbol_id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA()."}, "call": "PAYCALC1.cbl-c0", "check": {"claims": [{"both": false, "case": null, "evidence": null, "fetched": true, "from_pass": "a", "i": 0, "need": "the FD of PAY-FILE", "reason": "the excerpt does not show what AT END covers", "verdict": "UNVERIFIABLE", "witness": null}, {"both": false, "from_pass": "a", "i": 1, "verdict": "SUPPORTED"}], "n": "2"}, "claims": [{"i": 0, "lines": ["app/cbl/PAYCALC1.cbl:21-22"], "text": "the read reaches the end of `PAY-FILE`", "type": "condition"}, {"i": 1, "lines": ["app/cbl/PAYCALC1.cbl:22"], "text": "`WS-EOF` is set to `''Y''`", "type": "effect", "when": [0]}], "fetched": ["PAY-FILE"], "fetched_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy"], "id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#1", "kind": "rule", "m_fail": [], "name": "READ-PARA", "r": null, "sentence": null, "status": "open", "status1": "unverifiable", "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA()."}, "fetched_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy"], "statement": "Runs: READ-PARA (app/cbl/PAYCALC1.cbl:20-29)\n- When the read reaches the end of `PAY-FILE`: `WS-EOF` is set to `''Y''` [22]", "status": "open", "target_hash": "9b5154c88c14cdffb0266ff6c47e6882a838d3304d01ff9ee4e8e614a9e4b6d7"}',1.0,'','wicked-garden/domain-run',1795586989,'business_rule','unspecified','manual',0);
INSERT INTO "annotations" VALUES(3,6,'business_rule:ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#2','{"batch_hash": "c459054eb9df4c999782c61f54c4e791066b21442332cc71b23f67b5beb691f7", "content_hash": "5ce59a96041364cf5c331014c1686ef16f91e986766ad633725fb0bebc98088b", "contract": {"anchor": {"file": "app/cbl/PAYCALC1.cbl", "lines": [20, 29], "symbol_id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA()."}, "call": "PAYCALC1.cbl-c0", "claims": [{"i": 0, "lines": ["app/cbl/PAYCALC1.cbl:17"], "text": "performed until `WS-EOF = ''Y''`", "type": "trigger"}, {"i": 1, "lines": ["app/cbl/PAYCALC1.cbl:25-26"], "text": "`PAY-RATE` is read", "type": "effect", "when": [0]}], "id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#2", "kind": "rule", "m_fail": ["M1 claim 1 `when` [0] must list indices of this statement''s condition claims"], "name": "READ-PARA", "sentence": null, "status": "open", "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA()."}, "fetched_files": [], "statement": "Runs: READ-PARA (app/cbl/PAYCALC1.cbl:20-29)\n- Triggered by: performed until `WS-EOF = ''Y''` [17]\n- When performed until `WS-EOF = ''Y''`: `PAY-RATE` is read [25-26]", "status": "open", "target_hash": "9b5154c88c14cdffb0266ff6c47e6882a838d3304d01ff9ee4e8e614a9e4b6d7"}',1.0,'','wicked-garden/domain-run',1795586989,'business_rule','unspecified','manual',0);
INSERT INTO "annotations" VALUES(4,6,'business_rule:ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#3','{"batch_hash": "c459054eb9df4c999782c61f54c4e791066b21442332cc71b23f67b5beb691f7", "content_hash": "c38e72e5957526fa8adc4f1c0f5f6124e860229f1b9e920ecc1ff7a35a4ac5e5", "contract": {"anchor": {"file": "app/cbl/PAYCALC1.cbl", "lines": [20, 29], "symbol_id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA()."}, "call": "PAYCALC1.cbl-c0", "check": {"claims": [{"both": false, "from_pass": "a", "i": 0, "verdict": "SUPPORTED"}, {"both": true, "case": "VALUE", "from_pass": "a", "i": 1, "verdict": "OVERREACH", "witness": "`PAY-AMOUNT` is computed from the overtime rate, never zero (line 25)"}, {"both": true, "case": "VALUE", "from_pass": "b", "i": 1, "verdict": "OVERREACH", "witness": "`PAY-AMOUNT` is computed from the overtime rate, never zero (line 25)"}], "n": "3"}, "claims": [{"i": 0, "lines": ["app/cbl/PAYCALC1.cbl:24"], "text": "`PAY-HOURS > 40`", "type": "condition"}, {"i": 1, "lines": ["app/cbl/PAYCALC1.cbl:25-26"], "text": "`PAY-AMOUNT` is set to zero", "type": "effect", "when": [0]}], "fetched": [], "fetched_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy"], "id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#3", "kind": "rule", "m_fail": [], "name": "READ-PARA", "r": {"claims": [{"basis": null, "case": "VALUE", "cited_ok": null, "decision": "NOT-SENT", "i": 1, "lines": null, "reason": "non-SCOPE finding, or not COBOL: stands as found", "stands": true}, {"basis": null, "case": "VALUE", "cited_ok": null, "decision": "NOT-SENT", "i": 1, "lines": null, "reason": "non-SCOPE finding, or not COBOL: stands as found", "stands": true}], "n": "3", "no_result": false, "stands": true}, "repair": {"errors": ["repair returned no statement"], "n_statements": 0}, "sentence": null, "status": "open", "status1": "flagged", "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA()."}, "fetched_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy"], "statement": "Runs: READ-PARA (app/cbl/PAYCALC1.cbl:20-29)\n- When `PAY-HOURS > 40`: `PAY-AMOUNT` is set to zero [25-26]", "status": "open", "target_hash": "9b5154c88c14cdffb0266ff6c47e6882a838d3304d01ff9ee4e8e614a9e4b6d7"}',1.0,'','wicked-garden/domain-run',1795586989,'business_rule','unspecified','manual',0);
INSERT INTO "annotations" VALUES(5,5,'domain_target:ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().','{"anchor_symbol": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().", "batch_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy", "app/jcl/PAYJOB.jcl"], "batch_hash": "c459054eb9df4c999782c61f54c4e791066b21442332cc71b23f67b5beb691f7", "call": "PAYCALC1.cbl-c0", "file": "app/cbl/PAYCALC1.cbl", "lines": [15, 19], "not_rules": [{"call": "PAYCALC1.cbl-c0", "id": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().#nr0", "lines": "app/cbl/PAYCALC1.cbl:15-19", "reason": "control-only", "refused": true, "status": "refused", "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA()."}], "pieces": [["app/cbl/PAYCALC1.cbl", 15, 5, "1eeddfdd827343148f31517919314c35d4d4e6c3d9d2e73d7afaf15f9b6f65d2"]], "state": {"call": "PAYCALC1.cbl-c0", "tfails": ["M7 not_rule app/cbl/PAYCALC1.cbl:15-19 holds a decision on line(s) [17]: state it as a statement with that test as a condition claim", "M7 target lines not accounted by any statement anchor or not_rule: 15-19"], "unaccounted": true}, "statements": [], "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().", "target_hash": "ae775e68a24bd6aeffc5a7d035f2b6193937ad23feb48c245b334d66948f65d5", "text_sha": "1eeddfdd827343148f31517919314c35d4d4e6c3d9d2e73d7afaf15f9b6f65d2"}',1.0,'','wicked-garden/domain-run',1795586989,'domain_target','unspecified','manual',0);
INSERT INTO "annotations" VALUES(6,6,'domain_target:ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().','{"anchor_symbol": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().", "batch_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy", "app/jcl/PAYJOB.jcl"], "batch_hash": "c459054eb9df4c999782c61f54c4e791066b21442332cc71b23f67b5beb691f7", "call": "PAYCALC1.cbl-c0", "file": "app/cbl/PAYCALC1.cbl", "lines": [20, 29], "not_rules": [], "pieces": [["app/cbl/PAYCALC1.cbl", 15, 5, "1eeddfdd827343148f31517919314c35d4d4e6c3d9d2e73d7afaf15f9b6f65d2"], ["app/cbl/PAYCALC1.cbl", 20, 10, "959a10f8068e3d3796e91318d977809405dcc404d60ae3ca1e16d2aca046efde"]], "state": {"call": "PAYCALC1.cbl-c0", "tfails": [], "unaccounted": false}, "statements": ["ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#0", "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#1", "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#2", "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#3"], "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().", "target_hash": "9b5154c88c14cdffb0266ff6c47e6882a838d3304d01ff9ee4e8e614a9e4b6d7", "text_sha": "959a10f8068e3d3796e91318d977809405dcc404d60ae3ca1e16d2aca046efde"}',1.0,'','wicked-garden/domain-run',1795586989,'domain_target','unspecified','manual',0);
INSERT INTO "annotations" VALUES(7,23,'domain_target:app/jcl/PAYJOB.jcl:L1-L1','{"anchor_symbol": "jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB:", "batch_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy", "app/jcl/PAYJOB.jcl"], "batch_hash": "c459054eb9df4c999782c61f54c4e791066b21442332cc71b23f67b5beb691f7", "call": "PAYCALC1.cbl-c0", "file": "app/jcl/PAYJOB.jcl", "lines": [1, 1], "not_rules": [{"call": "PAYCALC1.cbl-c0", "id": "app/jcl/PAYJOB.jcl:L1-L1#nr0", "lines": "app/jcl/PAYJOB.jcl:1-1", "reason": "control-only", "status": "accepted", "target": "app/jcl/PAYJOB.jcl:L1-L1"}], "pieces": [["app/jcl/PAYJOB.jcl", 1, 1, "c87f9b9aa32fd4c1ee367e4738ee1ad76f0be43cd3df7f38689f24eda496411b"]], "state": {"call": "PAYCALC1.cbl-c0", "tfails": [], "unaccounted": false}, "statements": [], "target": "app/jcl/PAYJOB.jcl:L1-L1", "target_hash": "d487b8205758b40360c574a9dea0e3b92da22ff1076cf816d86a20fe2c720b28", "text_sha": "c87f9b9aa32fd4c1ee367e4738ee1ad76f0be43cd3df7f38689f24eda496411b"}',1.0,'','wicked-garden/domain-run',1795586989,'domain_target','unspecified','manual',0);
INSERT INTO "annotations" VALUES(8,24,'domain_target:jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:','{"anchor_symbol": "jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:", "batch_files": ["app/cbl/PAYCALC1.cbl", "app/cpy/PAYREC.cpy", "app/jcl/PAYJOB.jcl"], "batch_hash": "c459054eb9df4c999782c61f54c4e791066b21442332cc71b23f67b5beb691f7", "call": "PAYCALC1.cbl-c0", "file": "app/jcl/PAYJOB.jcl", "lines": [2, 4], "not_rules": [{"call": "PAYCALC1.cbl-c0", "id": "jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:#nr0", "lines": "app/jcl/PAYJOB.jcl:2-4", "reason": "control-only", "status": "accepted", "target": "jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:"}], "pieces": [["app/jcl/PAYJOB.jcl", 2, 3, "29db7b00214a10c1fc2fd041ddc74b4df757f6aba8f9d010829eba1bf9db7af0"]], "state": {"call": "PAYCALC1.cbl-c0", "tfails": [], "unaccounted": false}, "statements": [], "target": "jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:", "target_hash": "319ecbf5db92736662717f85f1c55bf81d7d185cfc80433aca27ab461e4dc15f", "text_sha": "29db7b00214a10c1fc2fd041ddc74b4df757f6aba8f9d010829eba1bf9db7af0"}',1.0,'','wicked-garden/domain-run',1795586989,'domain_target','unspecified','manual',0);
INSERT INTO "annotations" VALUES(9,6,'rule_group:6b50a714e01ef046','{"desc": "payroll", "group": {"area": "PAYCALC1", "details": [], "g": 1, "kind": "business", "rule": {"cites": ["ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#0"], "fate": "as written", "id": "g1.r", "text": "Hours over forty are paid at the overtime rate.", "verdict": "SUPPORTED"}, "title": "Overtime pay", "withheld": [{"cites": ["ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#0"], "id": "g1.d1", "layer": "detail", "reason": "OVERREACH : QUANTIFIER: the statements give no multiplier", "text": "Every overtime hour is paid double."}]}, "inputs": {"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#0": "6af18b6fa8d5e39cb3c03333668262eb0218060018fd2b3b5cb922cdc89e4dc2"}, "uncited": {}}',1.0,'','wicked-garden/domain-run',1795586989,'rule_group','unspecified','manual',0);
INSERT INTO "annotations" VALUES(10,2,'domain_followup:26bcf51c5a65e58e','{"both_passes": true, "case": "VALUE", "check": "Whether claim 1 holds: \"`PAY-AMOUNT` is set to zero\". The automatic repair failed its checks: repair returned no statement Reached from 1 site: PAYCALC1.cbl:17.", "kind": "flagged", "n": 1, "sites": [17], "statement": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#3", "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().", "unknown": "`PAY-AMOUNT` is computed from the overtime rate, never zero (line 25)", "where": {"file": "app/cbl/PAYCALC1.cbl", "lines": [20, 29]}}',1.0,'','wicked-garden/domain-run',1795586989,'domain_followup','unspecified','manual',0);
INSERT INTO "annotations" VALUES(11,2,'domain_followup:ccff866ff50d18fd','{"check": "the excerpt does not show what AT END covers Reached from 1 site: PAYCALC1.cbl:17.", "claim": 0, "kind": "open", "n": 2, "sites": [17], "statement": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#1", "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().", "unknown": "claim 0 (condition) \"the read reaches the end of `PAY-FILE`\" could not be confirmed from the excerpt; the checker needs: the FD of PAY-FILE", "where": {"file": "app/cbl/PAYCALC1.cbl", "lines": [20, 29]}}',1.0,'','wicked-garden/domain-run',1795586989,'domain_followup','unspecified','manual',0);
INSERT INTO "annotations" VALUES(12,2,'domain_followup:6930625d2a297312','{"check": "Run the extraction again, or confirm the statement against its cited lines.", "kind": "open", "n": 3, "sites": [17], "statement": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#2", "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().", "unknown": "the statement failed its mechanical checks: M1 claim 1 `when` [0] must list indices of this statement''s condition claims", "where": {"file": "app/cbl/PAYCALC1.cbl", "lines": [20, 29]}}',1.0,'','wicked-garden/domain-run',1795586989,'domain_followup','unspecified','manual',0);
INSERT INTO "annotations" VALUES(13,2,'domain_followup:0872db86655d2dc6','{"check": "State the decision in these lines as a rule, or confirm it is plumbing.", "kind": "refused", "n": 4, "sites": [], "target": "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().", "unknown": "not extracted: M7 not_rule app/cbl/PAYCALC1.cbl:15-19 holds a decision on line(s) [17]: state it as a statement with that test as a condition claim; M7 target lines not accounted by any statement anchor or not_rule: 15-19", "where": {"file": "app/cbl/PAYCALC1.cbl", "lines": [15, 19]}}',1.0,'','wicked-garden/domain-run',1795586989,'domain_followup','unspecified','manual',0);
CREATE TABLE cache (
  key     TEXT    NOT NULL,
  version INTEGER NOT NULL,
  value   TEXT    NOT NULL,
  PRIMARY KEY (key, version)
);
INSERT INTO "cache" VALUES('pagerank.top',1,'[["ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#",0.08635782],["cobol-copybook synthetic PAYREC:",0.08338198],["ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().",0.08338198],["ddname synthetic PAYCALC1.INFILE:",0.04667989],["ddname synthetic PAYCALC1.SYSOUT:",0.04667989],["jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB:",0.04667989],["jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:",0.04667989],["jcl-dataset synthetic PAY.HOURS.IN:",0.04667989],["ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().",0.04667989],["ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#PAY-LINE.",0.04667989],["ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#WS-EOF.",0.04667989],["ts-cobol . . . app/cpy/PAYREC/PAY-AMOUNT.",0.04667989],["ts-cobol . . . app/cpy/PAYREC/PAY-HOURS.",0.04667989],["ts-cobol . . . app/cpy/PAYREC/PAY-ID.",0.04667989],["ts-cobol . . . app/cpy/PAYREC/PAY-RATE.",0.04667989],["ts-cobol . . . app/cpy/PAYREC/PAY-RECORD.",0.04667989]]');
CREATE TABLE changes (
  seq    INTEGER PRIMARY KEY AUTOINCREMENT,
  op     TEXT    NOT NULL,
  target TEXT    NOT NULL,
  ts     INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);
INSERT INTO "changes" VALUES(1,'upsert','app/cpy/PAYREC.cpy',1795586989);
INSERT INTO "changes" VALUES(2,'upsert','app/cbl/PAYCALC1.cbl',1795586989);
INSERT INTO "changes" VALUES(3,'upsert','app/jcl/PAYJOB.jcl',1795586989);
CREATE TABLE content (
  git_sha TEXT PRIMARY KEY,
  blob    BLOB NOT NULL
);
INSERT INTO "content" VALUES('834543438a93c473718acde2f358b5a29bec2161',X'28B52FFD0058750B0056563C1B104D440FE733C9D9127E217EDB0B13FF76DC85C93811010005B00B320036003200BF5A6EDD635065EA83ECDD56B735DD96662B2ABA9C0745E4A83BCBAD1B288D729C2CAF13866EB23C24BA6EF69D4CD98A28FF69D130861F390EF272EBCBDA931C6F3393682EF032041C06000D3FB9505321C7D1F521549934BF7D4D92B13BAA4C66B9F53EA890FD01F8BEFF47BC07795BF30BE7123641B6EA10C552FC40656B3B3AE42BBC897129AEFDFFE35B46F314E486F02CAF428B635377788491D22045A6900838086E002EF0020319F87F81631F1FBCB5AD72DD96B2E7FF8983E00867DC5621E5211BA4B8FD62298E87558E34342050021190C41E1C07A902F288046E39A329AB9B3170BA96C5E6F2E506DA7CE6C2800F3B0734668D1197D834081D1902A50075013F4569FE354CD9A00278A50007F32EC0ADC1E18AE3CD9F7BC17201C224BA689E8957F1D5C10B066B0866A012A80A849F710900D713C2CA9328526D6E75BD07549898718136');
INSERT INTO "content" VALUES('6a4321f5d752ac9ffd7494f44cce50cd4c7184b2',X'28B52FFD00582D0300028716176087DCA8E066112ADF50479513407881399D450057E90A2670DCB2FD1AD1B4A40FB742055346B76513E3251FD6E808FD4F4B0998423487AED4977C0803BEA642FAFF83936B4433ACB5ED250F05C0A926716495FFC82A2F193102002F03DA352833');
INSERT INTO "content" VALUES('e61099ae8eaaddcaeedb0e818c1b688d2e29212e',X'28B52FFD0058C50200C4032020303120205041592D5245434F52442E0A20303549445049432058283829484F5552533928335241544520563939414D4F554E5436295639392E0A0B000029537D03241A2803741AAA67D0824D8D7D0E1CEC6A0116');
CREATE TABLE edge_base (
  source INTEGER NOT NULL,
  target INTEGER NOT NULL,
  kind   TEXT NOT NULL,
  file   TEXT NOT NULL DEFAULT '',
  data   TEXT NOT NULL,
  PRIMARY KEY (source, target, kind)
);
CREATE TABLE edge_history (
  archived_seq INTEGER PRIMARY KEY AUTOINCREMENT,
  git_sha      TEXT    NOT NULL DEFAULT '',
  file         TEXT    NOT NULL,
  edge_json    TEXT    NOT NULL
);
CREATE TABLE edge_supports (
  producer TEXT NOT NULL,
  snapshot TEXT NOT NULL,
  source   INTEGER NOT NULL,   -- sid, as edges.source
  target   INTEGER NOT NULL,   -- sid, as edges.target
  kind     TEXT NOT NULL,      -- serialized EdgeKind, as edges.kind
  fact_id  TEXT NOT NULL,
  data     TEXT NOT NULL,
  PRIMARY KEY (producer, snapshot, source, target, kind, fact_id)
);
CREATE TABLE edges (
  source         INTEGER NOT NULL,   -- sid FK → symbols.sid (dependent, edge-direction invariant)
  target         INTEGER NOT NULL,   -- sid FK → symbols.sid (dependency)
  kind           TEXT NOT NULL,      -- serialized EdgeKind (e.g. "calls")
  confidence     REAL NOT NULL,
  file           TEXT NOT NULL DEFAULT '', -- repo-relative source file of the edge site (Wave 2.6)
  data           TEXT NOT NULL,      -- full Edge as JSON (still carries string SymbolIds for round-trip)
  evidence_count INTEGER NOT NULL DEFAULT 0, -- queryable mirror of the first-class Edge.evidence_count field (brain consolidation)
  PRIMARY KEY (source, target, kind)
);
INSERT INTO "edges" VALUES(1,2,'"contains"',1.0,'app/cbl/PAYCALC1.cbl','{"source":"file . app/cbl/PAYCALC1.cbl:","target":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(1,3,'"contains"',1.0,'app/cbl/PAYCALC1.cbl','{"source":"file . app/cbl/PAYCALC1.cbl:","target":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#PAY-LINE.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(1,4,'"contains"',1.0,'app/cbl/PAYCALC1.cbl','{"source":"file . app/cbl/PAYCALC1.cbl:","target":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#WS-EOF.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(1,5,'"contains"',1.0,'app/cbl/PAYCALC1.cbl','{"source":"file . app/cbl/PAYCALC1.cbl:","target":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(1,6,'"contains"',1.0,'app/cbl/PAYCALC1.cbl','{"source":"file . app/cbl/PAYCALC1.cbl:","target":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(2,7,'"imports"',1.0,'app/cbl/PAYCALC1.cbl','{"source":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","target":"cobol-copybook synthetic PAYREC:","kind":"imports","confidence":1.0,"provenance":"parsed","resolved_by":"cobol-text","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":339,"end_byte":345,"start_line":11,"start_col":12,"end_line":11,"end_col":18}},"metadata":{"via":"copy"}}',0);
INSERT INTO "edges" VALUES(2,8,'{"other":"uses"}',1.0,'app/cbl/PAYCALC1.cbl','{"source":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","target":"ddname synthetic PAYCALC1.INFILE:","kind":{"other":"uses"},"confidence":1.0,"provenance":"parsed","resolved_by":"cobol-text","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":151,"end_byte":183,"start_line":5,"start_col":11,"end_line":5,"end_col":43}},"metadata":{"file":"PAY-FILE","via":"select-assign"}}',0);
INSERT INTO "edges" VALUES(23,24,'"contains"',1.0,'app/jcl/PAYJOB.jcl','{"source":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB:","target":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"jcl","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":1,"start_col":0,"end_line":1,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(24,8,'"contains"',1.0,'app/jcl/PAYJOB.jcl','{"source":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:","target":"ddname synthetic PAYCALC1.INFILE:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"jcl","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":2,"start_col":0,"end_line":2,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(8,26,'{"other":"uses"}',1.0,'app/jcl/PAYJOB.jcl','{"source":"ddname synthetic PAYCALC1.INFILE:","target":"jcl-dataset synthetic PAY.HOURS.IN:","kind":{"other":"uses"},"confidence":1.0,"provenance":"parsed","resolved_by":"jcl","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":2,"start_col":0,"end_line":2,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(24,26,'{"other":"uses"}',1.0,'app/jcl/PAYJOB.jcl','{"source":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:","target":"jcl-dataset synthetic PAY.HOURS.IN:","kind":{"other":"uses"},"confidence":1.0,"provenance":"parsed","resolved_by":"jcl","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":2,"start_col":0,"end_line":2,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(24,27,'"contains"',1.0,'app/jcl/PAYJOB.jcl','{"source":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:","target":"ddname synthetic PAYCALC1.SYSOUT:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"jcl","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":3,"start_col":0,"end_line":3,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(28,23,'"contains"',1.0,'app/jcl/PAYJOB.jcl','{"source":"file . app/jcl/PAYJOB.jcl:","target":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"grammarless","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(28,24,'"contains"',1.0,'app/jcl/PAYJOB.jcl','{"source":"file . app/jcl/PAYJOB.jcl:","target":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"grammarless","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(28,8,'"contains"',1.0,'app/jcl/PAYJOB.jcl','{"source":"file . app/jcl/PAYJOB.jcl:","target":"ddname synthetic PAYCALC1.INFILE:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"grammarless","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(28,26,'"contains"',1.0,'app/jcl/PAYJOB.jcl','{"source":"file . app/jcl/PAYJOB.jcl:","target":"jcl-dataset synthetic PAY.HOURS.IN:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"grammarless","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(28,27,'"contains"',1.0,'app/jcl/PAYJOB.jcl','{"source":"file . app/jcl/PAYJOB.jcl:","target":"ddname synthetic PAYCALC1.SYSOUT:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"grammarless","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(49,50,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"file . app/cpy/PAYREC.cpy:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-RECORD.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(49,51,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"file . app/cpy/PAYREC.cpy:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-ID.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(49,52,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"file . app/cpy/PAYREC.cpy:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-HOURS.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(49,53,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"file . app/cpy/PAYREC.cpy:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-RATE.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(49,54,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"file . app/cpy/PAYREC.cpy:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-AMOUNT.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(7,50,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"cobol-copybook synthetic PAYREC:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-RECORD.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"cobol-text","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(7,51,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"cobol-copybook synthetic PAYREC:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-ID.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"cobol-text","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(7,52,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"cobol-copybook synthetic PAYREC:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-HOURS.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"cobol-text","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(7,53,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"cobol-copybook synthetic PAYREC:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-RATE.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"cobol-text","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(7,54,'"contains"',1.0,'app/cpy/PAYREC.cpy','{"source":"cobol-copybook synthetic PAYREC:","target":"ts-cobol . . . app/cpy/PAYREC/PAY-AMOUNT.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"cobol-text","evidence_count":0,"location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(2,4,'"references"',0.64999997615814209,'app/cbl/PAYCALC1.cbl','{"source":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","target":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#WS-EOF.","kind":"references","confidence":0.65,"provenance":"import_map","resolved_by":"scoped-name-resolver","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":510,"end_byte":516,"start_line":16,"start_col":35,"end_line":16,"end_col":41}},"metadata":{"scope":"same-file"}}',0);
INSERT INTO "edges" VALUES(2,52,'"references"',0.60000002384185791,'app/cbl/PAYCALC1.cbl','{"source":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","target":"ts-cobol . . . app/cpy/PAYREC/PAY-HOURS.","kind":"references","confidence":0.6,"provenance":"import_map","resolved_by":"name-resolver","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":702,"end_byte":711,"start_line":23,"start_col":14,"end_line":23,"end_col":23}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(2,53,'"references"',0.60000002384185791,'app/cbl/PAYCALC1.cbl','{"source":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","target":"ts-cobol . . . app/cpy/PAYREC/PAY-RATE.","kind":"references","confidence":0.6,"provenance":"import_map","resolved_by":"name-resolver","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":753,"end_byte":761,"start_line":24,"start_col":36,"end_line":24,"end_col":44}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(2,6,'"calls"',0.64999997615814209,'app/cbl/PAYCALC1.cbl','{"source":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","target":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().","kind":"calls","confidence":0.65,"provenance":"import_map","resolved_by":"scoped-name-resolver","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":494,"end_byte":503,"start_line":16,"start_col":19,"end_line":16,"end_col":28}},"metadata":{"scope":"same-file"}}',0);
INSERT INTO "edges" VALUES(2,54,'"references"',0.60000002384185791,'app/cbl/PAYCALC1.cbl','{"source":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","target":"ts-cobol . . . app/cpy/PAYREC/PAY-AMOUNT.","kind":"references","confidence":0.6,"provenance":"import_map","resolved_by":"name-resolver","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":740,"end_byte":750,"start_line":24,"start_col":23,"end_line":24,"end_col":33}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(24,2,'"calls"',0.60000002384185791,'app/jcl/PAYJOB.jcl','{"source":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:","target":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","kind":"calls","confidence":0.6,"provenance":"import_map","resolved_by":"name-resolver","evidence_count":0,"location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":1,"start_col":0,"end_line":1,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(2,50,'"references"',0.60000002384185791,'app/cbl/PAYCALC1.cbl','{"source":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","target":"ts-cobol . . . app/cpy/PAYREC/PAY-RECORD.","kind":"references","confidence":0.6,"provenance":"import_map","resolved_by":"name-resolver","evidence_count":0,"location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":616,"end_byte":626,"start_line":20,"start_col":30,"end_line":20,"end_col":40}},"metadata":{}}',0);
CREATE TABLE embeddings (
  symbol TEXT PRIMARY KEY,  -- the string SymbolId (resolves via symbols.sym → symbols.sid → nodes.symbol)
  dim    INTEGER NOT NULL,  -- embedding dimensionality
  vec    BLOB    NOT NULL   -- little-endian f32 bytes, length = dim * 4
);
CREATE TABLE file_call_refs (
  path TEXT PRIMARY KEY,
  refs TEXT NOT NULL
);
INSERT INTO "file_call_refs" VALUES('app/cbl/PAYCALC1.cbl','[]');
INSERT INTO "file_call_refs" VALUES('app/jcl/PAYJOB.jcl','[]');
INSERT INTO "file_call_refs" VALUES('app/cpy/PAYREC.cpy','[]');
CREATE TABLE files (
  path    TEXT PRIMARY KEY,
  digest  TEXT NOT NULL DEFAULT '',
  git_sha TEXT
);
INSERT INTO "files" VALUES('app/cbl/PAYCALC1.cbl','a09edba27a168ec5','834543438a93c473718acde2f358b5a29bec2161');
INSERT INTO "files" VALUES('app/jcl/PAYJOB.jcl','34c72c2ea15b3399','6a4321f5d752ac9ffd7494f44cce50cd4c7184b2');
INSERT INTO "files" VALUES('app/cpy/PAYREC.cpy','ead62adc132c5933','e61099ae8eaaddcaeedb0e818c1b688d2e29212e');
CREATE TABLE meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);
INSERT INTO "meta" VALUES('graph_version','1');
INSERT INTO "meta" VALUES('redaction_epoch','1');
INSERT INTO "meta" VALUES('text_redaction_epoch','1');
INSERT INTO "meta" VALUES('fts_rowid','sid');
INSERT INTO "meta" VALUES('indexing','');
INSERT INTO "meta" VALUES('indexed_root','/fixture/mini-cobol');
INSERT INTO "meta" VALUES('indexed_at','2026-11-25T06:09:49Z');
INSERT INTO "meta" VALUES('repo_commit','');
INSERT INTO "meta" VALUES('repo_branch','');
INSERT INTO "meta" VALUES('repo_remote','');
INSERT INTO "meta" VALUES('repo_dirty','0');
INSERT INTO "meta" VALUES('unreadable_files','');
INSERT INTO "meta" VALUES('indexed_version','0.35.10');
INSERT INTO "meta" VALUES('extra_rules_digest','');
INSERT INTO "meta" VALUES('id_scheme','4');
INSERT INTO "meta" VALUES('plugin_overrides','');
INSERT INTO "meta" VALUES('dep_artifacts','[]');
INSERT INTO "meta" VALUES('dep_locks','{}');
INSERT INTO "meta" VALUES('graph_gen','1');
CREATE TABLE node_files (
  symbol INTEGER NOT NULL,           -- sid FK → symbols.sid
  file   TEXT    NOT NULL,           -- repo-relative contributing file
  is_def INTEGER NOT NULL DEFAULT 1, -- 1 = definition contribution, 0 = declaration
  data   TEXT    NOT NULL,           -- full Node JSON as this file's extraction produced it
  PRIMARY KEY (symbol, file)
);
INSERT INTO "node_files" VALUES(1,'app/cbl/PAYCALC1.cbl',1,'{"symbol":"file . app/cbl/PAYCALC1.cbl:","kind":"file","name":"app/cbl/PAYCALC1.cbl","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}');
INSERT INTO "node_files" VALUES(2,'app/cbl/PAYCALC1.cbl',1,'{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","kind":"module","name":"PAYCALC1","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":7,"end_byte":913,"start_line":0,"start_col":7,"end_line":28,"end_col":18}},"signature":"IDENTIFICATION DIVISION.","metadata":{}}');
INSERT INTO "node_files" VALUES(3,'app/cbl/PAYCALC1.cbl',1,'{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#PAY-LINE.","kind":"field","name":"PAY-LINE","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":256,"end_byte":293,"start_line":9,"start_col":7,"end_line":9,"end_col":44}},"signature":"01  PAY-LINE                PIC X(80)","metadata":{}}');
INSERT INTO "node_files" VALUES(4,'app/cbl/PAYCALC1.cbl',1,'{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#WS-EOF.","kind":"field","name":"WS-EOF","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":354,"end_byte":397,"start_line":12,"start_col":7,"end_line":12,"end_col":50}},"signature":"01  WS-EOF                  PIC X VALUE ''N''","metadata":{}}');
INSERT INTO "node_files" VALUES(5,'app/cbl/PAYCALC1.cbl',1,'{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().","kind":"function","name":"MAIN-PARA","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":433,"end_byte":567,"start_line":14,"start_col":7,"end_line":18,"end_col":18}},"signature":"MAIN-PARA.","metadata":{"behaviour":"real"}}');
INSERT INTO "node_files" VALUES(6,'app/cbl/PAYCALC1.cbl',1,'{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().","kind":"function","name":"READ-PARA","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":575,"end_byte":913,"start_line":19,"start_col":7,"end_line":28,"end_col":18}},"signature":"READ-PARA.","metadata":{"behaviour":"real"}}');
INSERT INTO "node_files" VALUES(7,'app/cbl/PAYCALC1.cbl',0,'{"symbol":"cobol-copybook synthetic PAYREC:","kind":{"other":"copybook"},"name":"PAYREC","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":339,"end_byte":345,"start_line":11,"start_col":12,"end_line":11,"end_col":18}},"metadata":{"boundary_reason":"COPY PAYREC: no copybook source named PAYREC is indexed","is_declaration":true}}');
INSERT INTO "node_files" VALUES(8,'app/cbl/PAYCALC1.cbl',0,'{"symbol":"ddname synthetic PAYCALC1.INFILE:","kind":{"other":"dd_binding"},"name":"PAYCALC1.INFILE","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":151,"end_byte":183,"start_line":5,"start_col":11,"end_line":5,"end_col":43}},"metadata":{"boundary_reason":"SELECT PAY-FILE ASSIGN TO INFILE: no JCL DD INFILE in a step that runs PAYCALC1 is indexed","ddname":"INFILE","file":"PAY-FILE","is_declaration":true,"program":"PAYCALC1"}}');
INSERT INTO "node_files" VALUES(23,'app/jcl/PAYJOB.jcl',1,'{"symbol":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB:","kind":"module","name":"PAYJOB","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":124,"start_line":0,"start_col":0,"end_line":3,"end_col":22}},"signature":"JOB","metadata":{}}');
INSERT INTO "node_files" VALUES(24,'app/jcl/PAYJOB.jcl',1,'{"symbol":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:","kind":{"other":"step"},"name":"STEP1","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":32,"end_byte":124,"start_line":1,"start_col":0,"end_line":3,"end_col":22}},"signature":"EXEC PGM=PAYCALC1","metadata":{}}');
INSERT INTO "node_files" VALUES(8,'app/jcl/PAYJOB.jcl',1,'{"symbol":"ddname synthetic PAYCALC1.INFILE:","kind":{"other":"dd_binding"},"name":"PAYCALC1.INFILE","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":2,"start_col":0,"end_line":2,"end_col":0}},"signature":"DD DSN=PAY.HOURS.IN,DISP=SHR","metadata":{"ddname":"INFILE","program":"PAYCALC1"}}');
INSERT INTO "node_files" VALUES(26,'app/jcl/PAYJOB.jcl',1,'{"symbol":"jcl-dataset synthetic PAY.HOURS.IN:","kind":{"other":"dataset"},"name":"PAY.HOURS.IN","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":2,"start_col":0,"end_line":2,"end_col":0}},"signature":"DATASET","metadata":{}}');
INSERT INTO "node_files" VALUES(27,'app/jcl/PAYJOB.jcl',1,'{"symbol":"ddname synthetic PAYCALC1.SYSOUT:","kind":{"other":"dd_binding"},"name":"PAYCALC1.SYSOUT","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":3,"start_col":0,"end_line":3,"end_col":0}},"signature":"DD SYSOUT=*","metadata":{"ddname":"SYSOUT","program":"PAYCALC1"}}');
INSERT INTO "node_files" VALUES(28,'app/jcl/PAYJOB.jcl',1,'{"symbol":"file . app/jcl/PAYJOB.jcl:","kind":"file","name":"app/jcl/PAYJOB.jcl","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}');
INSERT INTO "node_files" VALUES(49,'app/cpy/PAYREC.cpy',1,'{"symbol":"file . app/cpy/PAYREC.cpy:","kind":"file","name":"app/cpy/PAYREC.cpy","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}');
INSERT INTO "node_files" VALUES(50,'app/cpy/PAYREC.cpy',1,'{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-RECORD.","kind":"field","name":"PAY-RECORD","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":7,"end_byte":21,"start_line":0,"start_col":7,"end_line":0,"end_col":21}},"signature":"01  PAY-RECORD","metadata":{}}');
INSERT INTO "node_files" VALUES(51,'app/cpy/PAYREC.cpy',1,'{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-ID.","kind":"field","name":"PAY-ID","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":34,"end_byte":66,"start_line":1,"start_col":11,"end_line":1,"end_col":43}},"signature":"05  PAY-ID              PIC X(8)","metadata":{}}');
INSERT INTO "node_files" VALUES(52,'app/cpy/PAYREC.cpy',1,'{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-HOURS.","kind":"field","name":"PAY-HOURS","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":79,"end_byte":111,"start_line":2,"start_col":11,"end_line":2,"end_col":43}},"signature":"05  PAY-HOURS           PIC 9(3)","metadata":{}}');
INSERT INTO "node_files" VALUES(53,'app/cpy/PAYREC.cpy',1,'{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-RATE.","kind":"field","name":"PAY-RATE","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":124,"end_byte":159,"start_line":3,"start_col":11,"end_line":3,"end_col":46}},"signature":"05  PAY-RATE            PIC 9(3)V99","metadata":{}}');
INSERT INTO "node_files" VALUES(54,'app/cpy/PAYREC.cpy',1,'{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-AMOUNT.","kind":"field","name":"PAY-AMOUNT","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":172,"end_byte":207,"start_line":4,"start_col":11,"end_line":4,"end_col":46}},"signature":"05  PAY-AMOUNT          PIC 9(6)V99","metadata":{}}');
INSERT INTO "node_files" VALUES(7,'app/cpy/PAYREC.cpy',1,'{"symbol":"cobol-copybook synthetic PAYREC:","kind":{"other":"copybook"},"name":"PAYREC","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"signature":"COPYBOOK","metadata":{}}');
CREATE TABLE nodes (
  symbol                  INTEGER PRIMARY KEY,  -- sid FK → symbols.sid (ADR-002 stable identity)
  name                    TEXT NOT NULL,
  kind                    TEXT NOT NULL,        -- serialized NodeKind (e.g. "function")
  language                TEXT NOT NULL,
  file                    TEXT NOT NULL DEFAULT '', -- repo-relative source file (Wave 2.6 incremental)
  data                    TEXT NOT NULL,        -- full Node as JSON (still carries string SymbolId for round-trip)
  description             TEXT,                -- semantic: what is this symbol? (Semantic linking)
  requirement             TEXT,                -- semantic: requirement this symbol matches/fulfils
  requirement_validated   INTEGER NOT NULL DEFAULT 0, -- semantic: match validated as true (0/1)
  scope                   TEXT NOT NULL DEFAULT ''  -- hierarchical ownership/partition path (root='')
, requirement_validated_by TEXT, requirement_validated_at INTEGER);
INSERT INTO "nodes" VALUES(1,'app/cbl/PAYCALC1.cbl','"file"','cobol','app/cbl/PAYCALC1.cbl','{"symbol":"file . app/cbl/PAYCALC1.cbl:","kind":"file","name":"app/cbl/PAYCALC1.cbl","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(2,'PAYCALC1','"module"','cobol','app/cbl/PAYCALC1.cbl','{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#","kind":"module","name":"PAYCALC1","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":7,"end_byte":913,"start_line":0,"start_col":7,"end_line":28,"end_col":18}},"signature":"IDENTIFICATION DIVISION.","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(3,'PAY-LINE','"field"','cobol','app/cbl/PAYCALC1.cbl','{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#PAY-LINE.","kind":"field","name":"PAY-LINE","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":256,"end_byte":293,"start_line":9,"start_col":7,"end_line":9,"end_col":44}},"signature":"01  PAY-LINE                PIC X(80)","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(4,'WS-EOF','"field"','cobol','app/cbl/PAYCALC1.cbl','{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#WS-EOF.","kind":"field","name":"WS-EOF","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":354,"end_byte":397,"start_line":12,"start_col":7,"end_line":12,"end_col":50}},"signature":"01  WS-EOF                  PIC X VALUE ''N''","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(5,'MAIN-PARA','"function"','cobol','app/cbl/PAYCALC1.cbl','{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().","kind":"function","name":"MAIN-PARA","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":433,"end_byte":567,"start_line":14,"start_col":7,"end_line":18,"end_col":18}},"signature":"MAIN-PARA.","metadata":{"behaviour":"real"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(6,'READ-PARA','"function"','cobol','app/cbl/PAYCALC1.cbl','{"symbol":"ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().","kind":"function","name":"READ-PARA","language":"cobol","location":{"file":"app/cbl/PAYCALC1.cbl","span":{"start_byte":575,"end_byte":913,"start_line":19,"start_col":7,"end_line":28,"end_col":18}},"signature":"READ-PARA.","metadata":{"behaviour":"real"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(7,'PAYREC','{"other":"copybook"}','cobol','app/cpy/PAYREC.cpy','{"symbol":"cobol-copybook synthetic PAYREC:","kind":{"other":"copybook"},"name":"PAYREC","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"signature":"COPYBOOK","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(8,'PAYCALC1.INFILE','{"other":"dd_binding"}','jcl','app/jcl/PAYJOB.jcl','{"symbol":"ddname synthetic PAYCALC1.INFILE:","kind":{"other":"dd_binding"},"name":"PAYCALC1.INFILE","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":2,"start_col":0,"end_line":2,"end_col":0}},"signature":"DD DSN=PAY.HOURS.IN,DISP=SHR","metadata":{"ddname":"INFILE","program":"PAYCALC1"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(23,'PAYJOB','"module"','jcl','app/jcl/PAYJOB.jcl','{"symbol":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB:","kind":"module","name":"PAYJOB","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":124,"start_line":0,"start_col":0,"end_line":3,"end_col":22}},"signature":"JOB","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(24,'STEP1','{"other":"step"}','jcl','app/jcl/PAYJOB.jcl','{"symbol":"jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:","kind":{"other":"step"},"name":"STEP1","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":32,"end_byte":124,"start_line":1,"start_col":0,"end_line":3,"end_col":22}},"signature":"EXEC PGM=PAYCALC1","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(26,'PAY.HOURS.IN','{"other":"dataset"}','jcl','app/jcl/PAYJOB.jcl','{"symbol":"jcl-dataset synthetic PAY.HOURS.IN:","kind":{"other":"dataset"},"name":"PAY.HOURS.IN","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":2,"start_col":0,"end_line":2,"end_col":0}},"signature":"DATASET","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(27,'PAYCALC1.SYSOUT','{"other":"dd_binding"}','jcl','app/jcl/PAYJOB.jcl','{"symbol":"ddname synthetic PAYCALC1.SYSOUT:","kind":{"other":"dd_binding"},"name":"PAYCALC1.SYSOUT","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":3,"start_col":0,"end_line":3,"end_col":0}},"signature":"DD SYSOUT=*","metadata":{"ddname":"SYSOUT","program":"PAYCALC1"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(28,'app/jcl/PAYJOB.jcl','"file"','jcl','app/jcl/PAYJOB.jcl','{"symbol":"file . app/jcl/PAYJOB.jcl:","kind":"file","name":"app/jcl/PAYJOB.jcl","language":"jcl","location":{"file":"app/jcl/PAYJOB.jcl","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(49,'app/cpy/PAYREC.cpy','"file"','cobol','app/cpy/PAYREC.cpy','{"symbol":"file . app/cpy/PAYREC.cpy:","kind":"file","name":"app/cpy/PAYREC.cpy","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(50,'PAY-RECORD','"field"','cobol','app/cpy/PAYREC.cpy','{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-RECORD.","kind":"field","name":"PAY-RECORD","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":7,"end_byte":21,"start_line":0,"start_col":7,"end_line":0,"end_col":21}},"signature":"01  PAY-RECORD","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(51,'PAY-ID','"field"','cobol','app/cpy/PAYREC.cpy','{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-ID.","kind":"field","name":"PAY-ID","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":34,"end_byte":66,"start_line":1,"start_col":11,"end_line":1,"end_col":43}},"signature":"05  PAY-ID              PIC X(8)","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(52,'PAY-HOURS','"field"','cobol','app/cpy/PAYREC.cpy','{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-HOURS.","kind":"field","name":"PAY-HOURS","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":79,"end_byte":111,"start_line":2,"start_col":11,"end_line":2,"end_col":43}},"signature":"05  PAY-HOURS           PIC 9(3)","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(53,'PAY-RATE','"field"','cobol','app/cpy/PAYREC.cpy','{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-RATE.","kind":"field","name":"PAY-RATE","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":124,"end_byte":159,"start_line":3,"start_col":11,"end_line":3,"end_col":46}},"signature":"05  PAY-RATE            PIC 9(3)V99","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(54,'PAY-AMOUNT','"field"','cobol','app/cpy/PAYREC.cpy','{"symbol":"ts-cobol . . . app/cpy/PAYREC/PAY-AMOUNT.","kind":"field","name":"PAY-AMOUNT","language":"cobol","location":{"file":"app/cpy/PAYREC.cpy","span":{"start_byte":172,"end_byte":207,"start_line":4,"start_col":11,"end_line":4,"end_col":46}},"signature":"05  PAY-AMOUNT          PIC 9(6)V99","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
PRAGMA writable_schema=ON;
INSERT INTO sqlite_master(type,name,tbl_name,rootpage,sql)VALUES('table','nodes_fts','nodes_fts',0,'CREATE VIRTUAL TABLE nodes_fts USING fts5(
  symbol UNINDEXED,   -- string SymbolId used for join-back via symbols table; not tokenised
  name,               -- simple name
  signature,          -- optional type/parameter signature
  doc                 -- optional doc-comment
)');
CREATE TABLE 'nodes_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO "nodes_fts_config" VALUES('version',4);
CREATE TABLE 'nodes_fts_content'(id INTEGER PRIMARY KEY, c0, c1, c2, c3);
INSERT INTO "nodes_fts_content" VALUES(1,'file . app/cbl/PAYCALC1.cbl:','app/cbl/PAYCALC1.cbl','','');
INSERT INTO "nodes_fts_content" VALUES(2,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#','PAYCALC1','IDENTIFICATION DIVISION.','');
INSERT INTO "nodes_fts_content" VALUES(3,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#PAY-LINE.','PAY-LINE','01  PAY-LINE                PIC X(80)','');
INSERT INTO "nodes_fts_content" VALUES(4,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#WS-EOF.','WS-EOF','01  WS-EOF                  PIC X VALUE ''N''','');
INSERT INTO "nodes_fts_content" VALUES(5,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().','MAIN-PARA','MAIN-PARA.','');
INSERT INTO "nodes_fts_content" VALUES(6,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().','READ-PARA','READ-PARA.','');
INSERT INTO "nodes_fts_content" VALUES(7,'cobol-copybook synthetic PAYREC:','PAYREC','COPYBOOK','');
INSERT INTO "nodes_fts_content" VALUES(8,'ddname synthetic PAYCALC1.INFILE:','PAYCALC1.INFILE','DD DSN=PAY.HOURS.IN,DISP=SHR','');
INSERT INTO "nodes_fts_content" VALUES(23,'jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB:','PAYJOB','JOB','');
INSERT INTO "nodes_fts_content" VALUES(24,'jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:','STEP1','EXEC PGM=PAYCALC1','');
INSERT INTO "nodes_fts_content" VALUES(26,'jcl-dataset synthetic PAY.HOURS.IN:','PAY.HOURS.IN','DATASET','');
INSERT INTO "nodes_fts_content" VALUES(27,'ddname synthetic PAYCALC1.SYSOUT:','PAYCALC1.SYSOUT','DD SYSOUT=*','');
INSERT INTO "nodes_fts_content" VALUES(28,'file . app/jcl/PAYJOB.jcl:','app/jcl/PAYJOB.jcl','','');
INSERT INTO "nodes_fts_content" VALUES(49,'file . app/cpy/PAYREC.cpy:','app/cpy/PAYREC.cpy','','');
INSERT INTO "nodes_fts_content" VALUES(50,'ts-cobol . . . app/cpy/PAYREC/PAY-RECORD.','PAY-RECORD','01  PAY-RECORD','');
INSERT INTO "nodes_fts_content" VALUES(51,'ts-cobol . . . app/cpy/PAYREC/PAY-ID.','PAY-ID','05  PAY-ID              PIC X(8)','');
INSERT INTO "nodes_fts_content" VALUES(52,'ts-cobol . . . app/cpy/PAYREC/PAY-HOURS.','PAY-HOURS','05  PAY-HOURS           PIC 9(3)','');
INSERT INTO "nodes_fts_content" VALUES(53,'ts-cobol . . . app/cpy/PAYREC/PAY-RATE.','PAY-RATE','05  PAY-RATE            PIC 9(3)V99','');
INSERT INTO "nodes_fts_content" VALUES(54,'ts-cobol . . . app/cpy/PAYREC/PAY-AMOUNT.','PAY-AMOUNT','05  PAY-AMOUNT          PIC 9(6)V99','');
CREATE TABLE 'nodes_fts_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO "nodes_fts_data" VALUES(1,X'1300293F00');
INSERT INTO "nodes_fts_data" VALUES(10,X'0000000002041300041101010101010201010301010000');
INSERT INTO "nodes_fts_data" VALUES(137438953473,X'000000310830646174617365741A060102020105686F7572731A060101030102696E1A0601010401037061791A06010102040E0C09');
INSERT INTO "nodes_fts_data" VALUES(274877906945,X'0000002C033064641B06010202010870617963616C63311B0601010201067379736F75741B0C01010301020304090F');
INSERT INTO "nodes_fts_data" VALUES(412316860417,X'0000002604306170701C0601010201036A636C1C080101030401067061796A6F621C06010104040A0B');
INSERT INTO "nodes_fts_data" VALUES(2336462209025,X'0000027D03303031030601020201060102022E060102020201353306010202010601020201060102020106010202010133340601020701060102070101363606010207010138330601020702013003060102070101393406010206010601020601060102060106616D6F756E74360C0101030102040202707001060101023006010102010363626C01080101030402076F7079626F6F6B070601020202027079310801010304010264640806010202020369737008060102070306766973696F6E02060102030202736E08060102030103656F66040C010103010204020378656318060102020105686F75727308060102052C0C01010301020401026964330C010103010204030C656E74696669636174696F6E020601020202016E0806010206030466696C65080601010301036A6F62170601020201046C696E65030C01010301020401046D61696E050C01010201020201016E0406010208010470617261050C010103010203010C010103010203030179030C01010201020305060102042A0C010102010203010C010102010203010C010102010203010C010102010203010C010102010203040563616C6331010601010401060101020606010102100601020404036A6F621706010102040372656307060101022A060101040202676D180601020302026963030601020501060102052F06010205010601020501060102050106010205010472617465350C0101030102040203656164060C0101020102020304636F7264320C0101030102040103736872080601020802047465703118060101020103763939350601020801060102080204616C7565040601020701027773040C010102010203010178030601020601060102062F060102060413170D08080812100E0B0E0A090A0D090D0A140C13080B0A0E0E0816381B0A0F09220E0D0E0A0B0F0B0C');
CREATE TABLE 'nodes_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
INSERT INTO "nodes_fts_docsize" VALUES(1,X'00040000');
INSERT INTO "nodes_fts_docsize" VALUES(2,X'00010200');
INSERT INTO "nodes_fts_docsize" VALUES(3,X'00020600');
INSERT INTO "nodes_fts_docsize" VALUES(4,X'00020700');
INSERT INTO "nodes_fts_docsize" VALUES(5,X'00020200');
INSERT INTO "nodes_fts_docsize" VALUES(6,X'00020200');
INSERT INTO "nodes_fts_docsize" VALUES(7,X'00010100');
INSERT INTO "nodes_fts_docsize" VALUES(8,X'00020700');
INSERT INTO "nodes_fts_docsize" VALUES(23,X'00010100');
INSERT INTO "nodes_fts_docsize" VALUES(24,X'00010300');
INSERT INTO "nodes_fts_docsize" VALUES(26,X'00030100');
INSERT INTO "nodes_fts_docsize" VALUES(27,X'00020200');
INSERT INTO "nodes_fts_docsize" VALUES(28,X'00040000');
INSERT INTO "nodes_fts_docsize" VALUES(49,X'00040000');
INSERT INTO "nodes_fts_docsize" VALUES(50,X'00020300');
INSERT INTO "nodes_fts_docsize" VALUES(51,X'00020600');
INSERT INTO "nodes_fts_docsize" VALUES(52,X'00020600');
INSERT INTO "nodes_fts_docsize" VALUES(53,X'00020700');
INSERT INTO "nodes_fts_docsize" VALUES(54,X'00020700');
CREATE TABLE 'nodes_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
INSERT INTO "nodes_fts_idx" VALUES(1,X'',2);
INSERT INTO "nodes_fts_idx" VALUES(2,X'',2);
INSERT INTO "nodes_fts_idx" VALUES(3,X'',2);
INSERT INTO "nodes_fts_idx" VALUES(17,X'',2);
CREATE TABLE search_misses (
  query       TEXT    NOT NULL,
  searched_at INTEGER NOT NULL, -- epoch millis
  session_id  TEXT
);
CREATE TABLE support_owners (
  producer   TEXT NOT NULL,
  snapshot   TEXT NOT NULL,
  generation INTEGER NOT NULL,
  PRIMARY KEY (producer, snapshot)
);
CREATE TABLE symbols (
  sid      INTEGER PRIMARY KEY AUTOINCREMENT,
  sym      TEXT UNIQUE NOT NULL,  -- the UNIQUE constraint implies an index; no separate idx needed
  gen      INTEGER NOT NULL DEFAULT 0,  -- live-node epoch (M8/DoD-XA4): bumped on reuse-after-delete
  had_node INTEGER NOT NULL DEFAULT 0   -- sticky 1 once a node has EVER existed for this sid; the
                                        -- durable signal that distinguishes a reuse-after-delete
                                        -- (had_node=1, no live node) from a first-ever / edge-only
                                        -- symbol getting its first node (had_node=0, no bump).
);
INSERT INTO "symbols" VALUES(1,'file . app/cbl/PAYCALC1.cbl:',0,1);
INSERT INTO "symbols" VALUES(2,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#',0,1);
INSERT INTO "symbols" VALUES(3,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#PAY-LINE.',0,1);
INSERT INTO "symbols" VALUES(4,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#WS-EOF.',0,1);
INSERT INTO "symbols" VALUES(5,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().',0,1);
INSERT INTO "symbols" VALUES(6,'ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().',0,1);
INSERT INTO "symbols" VALUES(7,'cobol-copybook synthetic PAYREC:',0,1);
INSERT INTO "symbols" VALUES(8,'ddname synthetic PAYCALC1.INFILE:',0,1);
INSERT INTO "symbols" VALUES(23,'jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB:',0,1);
INSERT INTO "symbols" VALUES(24,'jcl synthetic app/jcl/PAYJOB.jcl::PAYJOB::STEP1:',0,1);
INSERT INTO "symbols" VALUES(26,'jcl-dataset synthetic PAY.HOURS.IN:',0,1);
INSERT INTO "symbols" VALUES(27,'ddname synthetic PAYCALC1.SYSOUT:',0,1);
INSERT INTO "symbols" VALUES(28,'file . app/jcl/PAYJOB.jcl:',0,1);
INSERT INTO "symbols" VALUES(49,'file . app/cpy/PAYREC.cpy:',0,1);
INSERT INTO "symbols" VALUES(50,'ts-cobol . . . app/cpy/PAYREC/PAY-RECORD.',0,1);
INSERT INTO "symbols" VALUES(51,'ts-cobol . . . app/cpy/PAYREC/PAY-ID.',0,1);
INSERT INTO "symbols" VALUES(52,'ts-cobol . . . app/cpy/PAYREC/PAY-HOURS.',0,1);
INSERT INTO "symbols" VALUES(53,'ts-cobol . . . app/cpy/PAYREC/PAY-RATE.',0,1);
INSERT INTO "symbols" VALUES(54,'ts-cobol . . . app/cpy/PAYREC/PAY-AMOUNT.',0,1);
CREATE TABLE unresolved_refs (
  id       INTEGER PRIMARY KEY,
  from_sym INTEGER NOT NULL,     -- sid FK → symbols.sid of the referencing symbol
  raw_name TEXT NOT NULL,        -- the written name at the call/import site
  kind     TEXT NOT NULL,        -- serialized EdgeKind (same encoding as edges.kind)
  file     TEXT NOT NULL DEFAULT '', -- repo-relative source file (Wave 2.6 incremental)
  line     INTEGER NOT NULL DEFAULT 0, -- start_line of the reference site
  start_byte INTEGER NOT NULL DEFAULT 0, -- byte offset of the site start (0 = unknown/synthetic)
  end_byte   INTEGER NOT NULL DEFAULT 0  -- byte offset of the site end   (0 = unknown/synthetic)
);
CREATE INDEX idx_nodes_name ON nodes(name);
CREATE INDEX idx_nodes_name_folded ON nodes(lower(name));
CREATE INDEX idx_nodes_kind ON nodes(kind);
CREATE INDEX idx_nodes_file ON nodes(file);
CREATE INDEX idx_nodes_scope ON nodes(scope);
CREATE INDEX idx_node_files_file ON node_files(file);
CREATE INDEX idx_edges_target ON edges(target);
CREATE INDEX idx_edges_source ON edges(source);
CREATE INDEX idx_edges_file ON edges(file);
CREATE INDEX idx_edge_supports_key ON edge_supports(source, target, kind);
CREATE INDEX idx_edge_base_file ON edge_base(file);
CREATE INDEX idx_unresolved_refs_name ON unresolved_refs(raw_name);
CREATE INDEX idx_unresolved_refs_file ON unresolved_refs(file);
CREATE INDEX idx_unresolved_refs_from ON unresolved_refs(from_sym);
CREATE INDEX idx_edge_history_file ON edge_history(file);
CREATE INDEX idx_annotations_node ON annotations(node_sym);
CREATE INDEX idx_annotations_key  ON annotations(key);
CREATE INDEX idx_annotations_type ON annotations(type);
CREATE INDEX idx_annotations_last_verified ON annotations(last_verified);
CREATE INDEX idx_access_item    ON access_log(item_id);
CREATE INDEX idx_access_session ON access_log(session_id);
CREATE INDEX idx_search_misses_time ON search_misses(searched_at);
CREATE TRIGGER nodes_fts_rowid_guard AFTER INSERT ON nodes_fts_content WHEN NEW.id IS NOT (SELECT sid FROM symbols WHERE sym = NEW.c0) BEGIN DELETE FROM meta WHERE k = 'fts_rowid'; END;
PRAGMA writable_schema=OFF;
DELETE FROM "sqlite_sequence";
INSERT INTO "sqlite_sequence" VALUES('symbols',89);
INSERT INTO "sqlite_sequence" VALUES('changes',3);
INSERT INTO "sqlite_sequence" VALUES('annotations',13);
COMMIT;
