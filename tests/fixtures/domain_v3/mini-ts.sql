-- J6 C2 real-shape fixture: the estate store after garden's domain-run wrote one run (DES-C2-v2 F27, R23).
-- Writer: garden 2b4066d71410b5c08d46971de0a378cfb0824d76 scripts/domain/store.py rows_for (:303-381) through wicked-estate 0.35.10 write-batch.
-- Generator: tests/fixtures/domain_v3/gen/gen_ts.py sha256 c12a0b8e70a821d55a2cf983b2a5a655a108b980b0f563356184d5ce05f22fd4; seat answers are scenario fakes.
-- tree src/api/routes.ts sha256 062e9db57296ebccc100dde32a9de611da83aaf18789507d35edd51bdd25f64c
-- tree src/core/launch-posture.ts sha256 d70082b93e5f66e82414e55de923082620970be186a5f67f754007672af52148
-- tree src/core/launch.ts sha256 6a90c27b1c14e7877afb0c6ce1c57cb3bd00b0cce288947c64d78c758c19dc3e
-- tree src/core/runs.ts sha256 3303dd60eb875e0ffd71f3344b214aab155d5c41c863cd5809bcba2f6fa0143e
-- tree tests/launch-posture.test.ts sha256 ad763e13b43c74d15a0a98129ada1e2e47f46088c32d2cdc538df7e2a9eea97f
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
INSERT INTO "annotations" VALUES(1,17,'business_rule:ts-typescript . . . src/core/launch-posture/launchPosture().#0','{"batch_hash": "30d4e4830d11a264c696489d2d1fa6d02beb7fc9460bbf1d86427aee175981e3", "content_hash": "f4d0679a8a2ef6dbc0b0083ab09f19425def4d4ce3af2d005a21a24d11a7f46d", "contract": {"anchor": {"file": "src/core/launch-posture.ts", "lines": [3, 8], "symbol_id": "ts-typescript . . . src/core/launch-posture/launchPosture()."}, "call": "call0", "check": {"claims": [{"both": false, "from_pass": "a", "i": 0, "verdict": "SUPPORTED"}, {"both": false, "from_pass": "a", "i": 1, "verdict": "SUPPORTED"}, {"both": false, "from_pass": "a", "i": 2, "verdict": "SUPPORTED"}], "n": "1"}, "claims": [{"i": 0, "lines": ["src/core/launch-posture.ts:4"], "text": "`spec.power && process.env.WICKED_POWER === ''1''`", "type": "condition"}, {"i": 1, "lines": ["src/core/launch-posture.ts:5"], "text": "returns `''power''`", "type": "effect", "when": [0]}, {"i": 2, "lines": ["src/core/launch-posture.ts:7"], "text": "returns `PHASED`", "type": "else"}], "fetched": [], "fetched_files": ["src/core/launch-posture.ts"], "id": "ts-typescript . . . src/core/launch-posture/launchPosture().#0", "kind": "rule", "m_fail": [], "name": "launchPosture", "r": null, "sentence": null, "status": "Checked", "status1": "Checked", "target": "ts-typescript . . . src/core/launch-posture/launchPosture()."}, "fetched_files": ["src/core/launch-posture.ts"], "statement": "Runs: launchPosture (src/core/launch-posture.ts:3-8)\n- When `spec.power && process.env.WICKED_POWER === ''1''`: returns `''power''` [5]\n- Always: returns `PHASED` [7]", "status": "Checked", "target_hash": "3aa768ffa2953b1d03e1ddc48ca3f0adbb1b718aab29912cbe731f85549ca3c4"}',1.0,'','wicked-garden/domain-run',1795586990,'business_rule','unspecified','manual',0);
INSERT INTO "annotations" VALUES(2,17,'domain_target:ts-typescript . . . src/core/launch-posture/launchPosture().','{"anchor_symbol": "ts-typescript . . . src/core/launch-posture/launchPosture().", "batch_files": ["src/core/launch-posture.ts"], "batch_hash": "30d4e4830d11a264c696489d2d1fa6d02beb7fc9460bbf1d86427aee175981e3", "call": "call0", "file": "src/core/launch-posture.ts", "lines": [3, 8], "not_rules": [], "pieces": [["src/core/launch-posture.ts", 3, 6, "e566e83a0d788ba26d7f0b8a81b830d1b3d5e98de91998bb5f395e555be3567a"], ["src/core/launch-posture.ts", 7, 1, "80bc4d869b8a177a9794747db577a7f79ce01325073b67044b45e0976e0551a3"]], "state": {"call": "call0", "tfails": [], "unaccounted": false}, "statements": ["ts-typescript . . . src/core/launch-posture/launchPosture().#0"], "target": "ts-typescript . . . src/core/launch-posture/launchPosture().", "target_hash": "3aa768ffa2953b1d03e1ddc48ca3f0adbb1b718aab29912cbe731f85549ca3c4", "text_sha": "e566e83a0d788ba26d7f0b8a81b830d1b3d5e98de91998bb5f395e555be3567a"}',1.0,'','wicked-garden/domain-run',1795586990,'domain_target','unspecified','manual',0);
INSERT INTO "annotations" VALUES(3,17,'rule_group:bdb84bd7ee172a17','{"desc": "how a launch picks its posture", "group": {"area": "launch-posture", "details": [], "g": 1, "kind": "housekeeping", "rule": {"cites": ["ts-typescript . . . src/core/launch-posture/launchPosture().#0"], "fate": "as written", "id": "g1.r", "text": "A launch that is not a power launch runs phased.", "verdict": "SUPPORTED"}, "title": "Posture fallback", "withheld": []}, "inputs": {"ts-typescript . . . src/core/launch-posture/launchPosture().#0": "71a03219a640cca58f5abf2662c62cf22988e052eabdc0e0e945a1c3558271f0"}, "uncited": {}}',1.0,'','wicked-garden/domain-run',1795586990,'rule_group','unspecified','manual',0);
INSERT INTO "annotations" VALUES(4,17,'rule_group:73a63cccb59fb1cf','{"desc": "how a launch picks its posture", "group": {"area": "launch-posture", "details": [], "g": 2, "kind": "business", "rule": {"cites": ["ts-typescript . . . src/core/launch-posture/launchPosture().#0"], "fate": "as written", "id": "g2.r", "text": "A launch runs in power mode only when power is asked for and enabled.", "verdict": "SUPPORTED"}, "title": "Power launches", "withheld": []}, "inputs": {"ts-typescript . . . src/core/launch-posture/launchPosture().#0": "71a03219a640cca58f5abf2662c62cf22988e052eabdc0e0e945a1c3558271f0"}, "uncited": {}}',1.0,'','wicked-garden/domain-run',1795586990,'rule_group','unspecified','manual',0);
CREATE TABLE cache (
  key     TEXT    NOT NULL,
  version INTEGER NOT NULL,
  value   TEXT    NOT NULL,
  PRIMARY KEY (key, version)
);
INSERT INTO "cache" VALUES('pagerank.top',1,'[["ts-typescript . . . src/core/launch-posture/launchPosture().",0.11490365],["ts-typescript . . . src/core/launch/startRun().",0.053487226],["ts-typescript . . . src/core/runs/listRuns().",0.053487226],["ts-typescript . . . src/api/routes/register().",0.037534896],["ts-typescript . . . src/core/launch-posture/PHASED.",0.037534896],["ts-typescript . . . tests/launch-posture.test/launchPostureCase().",0.037534896]]');
CREATE TABLE changes (
  seq    INTEGER PRIMARY KEY AUTOINCREMENT,
  op     TEXT    NOT NULL,
  target TEXT    NOT NULL,
  ts     INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);
INSERT INTO "changes" VALUES(1,'upsert','src/core/launch-posture.ts',1795586989);
INSERT INTO "changes" VALUES(2,'upsert','src/api/routes.ts',1795586989);
INSERT INTO "changes" VALUES(3,'upsert','tests/launch-posture.test.ts',1795586989);
INSERT INTO "changes" VALUES(4,'upsert','src/core/runs.ts',1795586989);
INSERT INTO "changes" VALUES(5,'upsert','src/core/launch.ts',1795586989);
CREATE TABLE content (
  git_sha TEXT PRIMARY KEY,
  blob    BLOB NOT NULL
);
INSERT INTO "content" VALUES('1d25949e2425573d78b910097027710d6e1f4260',X'28B52FFD0058FD0300D247191B606BDBF8A8951B2DF21FAA251D11826D80B87643F5752280C30E0CD70EB500F6F28F02D9739B71FF4348E1E7959AFE6347DA33414635F1635864A83AFB49A816201BC19BF07AEAC2295B69E01D3A94803038DEE82A6FC76D5E7A58A4EC84393BAFAA660908005CCBB1C53C1057A843E26382835689C273E0EC309F');
INSERT INTO "content" VALUES('2b5cf91f17b09c1bc34fd8cbd5f043c502264322',X'28B52FFD0058B101006578706F72742066756E6374696F6E206C69737452756E7328293A20737472696E675B5D207B0A202072657475726E205B5D3B0A7D0A');
INSERT INTO "content" VALUES('5f0c4344b12b2607730b3d0118cbe1260d3d7d68',X'28B52FFD0058D50400D249212140A53A076EA855BB176E4246AD15CE61989341F80F244EC3060AAD5D36C30B6B4C7A29A3D7AF0C636738E5C23F6103F6D6DA170EA4781A8D9330202025805789715DC37704B97289FAD53A005350C62FC9758EB6EA767C21C015E877EB8BDB17050BA78CFD4C8E67549D9B928EA77409FB0DAC2AC31E1BA3E9B1B7370EA370782E23B70B0600333340CE325C1DEA56ED979213DDA5CE');
INSERT INTO "content" VALUES('29d850223e0a27728aa1dc423403d800711abbfb',X'28B52FFD0058CD040072891E1D6047AC03019A5156BEA450AF2294C1D48364C174A83454077490B76505A7394CA9440C5789ACDDD6FC872966E009AC253929C74EFE8961494E0740A9E1DBB5DBFE33B862B43D60B725CE860E49BE3ED786C6A320CF61086A64A8B168E892C31081A1B12542F23384B5D095C7C79B6B2C9A5BC30FDEFC5671010A002B2BC812B848B53A321A8F6A8D2807D5153516DA0E269FD35419');
INSERT INTO "content" VALUES('87b437d06112439cc4140957917bf0883bd0d5c3',X'28B52FFD00582D03000286141990B7390BBF8A9C86844C3D270BCDBD77503F5267A5C58BD80CD7EE289A71AF0E1C0DA9CC88AFE2ED632E085C8820A5A2286D72C0BBC328C0C1273AE8EB1886F1924FDC8C5F5D3920A0B448C1CCFA548B1205008476EB08C981208A5103AF0CAA0C');
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
INSERT INTO "edges" VALUES(1,2,'"contains"',1.0,'src/core/launch.ts','{"source":"file . src/core/launch.ts:","target":"ts-typescript . . . src/core/launch/startRun().","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(1,3,'"imports"',1.0,'src/core/launch.ts','{"source":"file . src/core/launch.ts:","target":"ts-typescript . . . import/./launch-posture/","kind":"imports","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch.ts","span":{"start_byte":30,"end_byte":48,"start_line":0,"start_col":30,"end_line":0,"end_col":48}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(1,4,'"contains"',1.0,'src/core/launch.ts','{"source":"file . src/core/launch.ts:","target":"value synthetic ts-typescript . . . src/core/launch/startRun().:param:spec:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(11,12,'"contains"',1.0,'src/core/runs.ts','{"source":"file . src/core/runs.ts:","target":"ts-typescript . . . src/core/runs/listRuns().","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/runs.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(15,16,'"contains"',1.0,'src/core/launch-posture.ts','{"source":"file . src/core/launch-posture.ts:","target":"ts-typescript . . . src/core/launch-posture/PHASED.","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(15,17,'"contains"',1.0,'src/core/launch-posture.ts','{"source":"file . src/core/launch-posture.ts:","target":"ts-typescript . . . src/core/launch-posture/launchPosture().","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(15,18,'"contains"',1.0,'src/core/launch-posture.ts','{"source":"file . src/core/launch-posture.ts:","target":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:param:spec:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(15,19,'"contains"',1.0,'src/core/launch-posture.ts','{"source":"file . src/core/launch-posture.ts:","target":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:return:value:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(15,20,'"contains"',1.0,'src/core/launch-posture.ts','{"source":"file . src/core/launch-posture.ts:","target":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:local:PHASED:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(19,20,'{"other":"flows_to"}',1.0,'src/core/launch-posture.ts','{"source":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:return:value:","target":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:local:PHASED:","kind":{"other":"flows_to"},"confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/core/launch-posture.ts","span":{"start_byte":182,"end_byte":188,"start_line":6,"start_col":9,"end_line":6,"end_col":15}},"metadata":{"construct":"return","constructs":["return"],"flow_evidence":["syntax"],"flow_rules":["typescript/syntax/return"],"flow_semantics":["value_preserving"],"flow_support":[{"confidence":1.0,"construct":"return","end_byte":188,"evidence":"syntax","file":"src/core/launch-posture.ts","line":6,"provenance":"parsed","resolved_by":"tree-sitter","rule":"typescript/syntax/return","semantics":"value_preserving","start_byte":182}]}}',0);
INSERT INTO "edges" VALUES(33,34,'"contains"',1.0,'src/api/routes.ts','{"source":"file . src/api/routes.ts:","target":"ts-typescript . . . src/api/routes/register().","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/api/routes.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(33,35,'"imports"',1.0,'src/api/routes.ts','{"source":"file . src/api/routes.ts:","target":"ts-typescript . . . import/../core/launch/","kind":"imports","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/api/routes.ts","span":{"start_byte":25,"end_byte":41,"start_line":0,"start_col":25,"end_line":0,"end_col":41}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(33,36,'"imports"',1.0,'src/api/routes.ts','{"source":"file . src/api/routes.ts:","target":"ts-typescript . . . import/../core/runs/","kind":"imports","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/api/routes.ts","span":{"start_byte":68,"end_byte":82,"start_line":1,"start_col":25,"end_line":1,"end_col":39}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(33,37,'"contains"',1.0,'src/api/routes.ts','{"source":"file . src/api/routes.ts:","target":"value synthetic ts-typescript . . . src/api/routes/register().:param:app:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"src/api/routes.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(46,47,'"contains"',1.0,'tests/launch-posture.test.ts','{"source":"file . tests/launch-posture.test.ts:","target":"ts-typescript . . . tests/launch-posture.test/launchPostureCase().","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(46,48,'"imports"',1.0,'tests/launch-posture.test.ts','{"source":"file . tests/launch-posture.test.ts:","target":"ts-typescript . . . import/../src/core/launch-posture/","kind":"imports","confidence":1.0,"provenance":"parsed","resolved_by":"tree-sitter","evidence_count":0,"location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":30,"end_byte":58,"start_line":0,"start_col":30,"end_line":0,"end_col":58}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(2,17,'"calls"',0.62999999523162842,'src/core/launch.ts','{"source":"ts-typescript . . . src/core/launch/startRun().","target":"ts-typescript . . . src/core/launch-posture/launchPosture().","kind":"calls","confidence":0.63,"provenance":"import_map","resolved_by":"import-map-resolver","evidence_count":0,"location":{"file":"src/core/launch.ts","span":{"start_byte":123,"end_byte":142,"start_line":3,"start_col":18,"end_line":3,"end_col":37}},"metadata":{"via":"import-map"}}',0);
INSERT INTO "edges" VALUES(47,17,'"calls"',0.62999999523162842,'tests/launch-posture.test.ts','{"source":"ts-typescript . . . tests/launch-posture.test/launchPostureCase().","target":"ts-typescript . . . src/core/launch-posture/launchPosture().","kind":"calls","confidence":0.63,"provenance":"import_map","resolved_by":"import-map-resolver","evidence_count":0,"location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":108,"end_byte":138,"start_line":3,"start_col":9,"end_line":3,"end_col":39}},"metadata":{"via":"import-map"}}',0);
INSERT INTO "edges" VALUES(33,1,'"imports"',0.89999997615814209,'src/api/routes.ts','{"source":"file . src/api/routes.ts:","target":"file . src/core/launch.ts:","kind":"imports","confidence":0.9,"provenance":"import_map","resolved_by":"relative-import","evidence_count":0,"location":{"file":"src/api/routes.ts","span":{"start_byte":25,"end_byte":41,"start_line":0,"start_col":25,"end_line":0,"end_col":41}},"metadata":{"rule":"probe","via":"relative-path"}}',0);
INSERT INTO "edges" VALUES(34,2,'"calls"',0.62999999523162842,'src/api/routes.ts','{"source":"ts-typescript . . . src/api/routes/register().","target":"ts-typescript . . . src/core/launch/startRun().","kind":"calls","confidence":0.63,"provenance":"import_map","resolved_by":"import-map-resolver","evidence_count":0,"location":{"file":"src/api/routes.ts","span":{"start_byte":169,"end_byte":187,"start_line":5,"start_col":11,"end_line":5,"end_col":29}},"metadata":{"via":"import-map"}}',0);
INSERT INTO "edges" VALUES(1,15,'"imports"',0.89999997615814209,'src/core/launch.ts','{"source":"file . src/core/launch.ts:","target":"file . src/core/launch-posture.ts:","kind":"imports","confidence":0.9,"provenance":"import_map","resolved_by":"relative-import","evidence_count":0,"location":{"file":"src/core/launch.ts","span":{"start_byte":30,"end_byte":48,"start_line":0,"start_col":30,"end_line":0,"end_col":48}},"metadata":{"rule":"probe","via":"relative-path"}}',0);
INSERT INTO "edges" VALUES(33,11,'"imports"',0.89999997615814209,'src/api/routes.ts','{"source":"file . src/api/routes.ts:","target":"file . src/core/runs.ts:","kind":"imports","confidence":0.9,"provenance":"import_map","resolved_by":"relative-import","evidence_count":0,"location":{"file":"src/api/routes.ts","span":{"start_byte":68,"end_byte":82,"start_line":1,"start_col":25,"end_line":1,"end_col":39}},"metadata":{"rule":"probe","via":"relative-path"}}',0);
INSERT INTO "edges" VALUES(46,15,'"imports"',0.89999997615814209,'tests/launch-posture.test.ts','{"source":"file . tests/launch-posture.test.ts:","target":"file . src/core/launch-posture.ts:","kind":"imports","confidence":0.9,"provenance":"import_map","resolved_by":"relative-import","evidence_count":0,"location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":30,"end_byte":58,"start_line":0,"start_col":30,"end_line":0,"end_col":58}},"metadata":{"rule":"probe","via":"relative-path"}}',0);
INSERT INTO "edges" VALUES(34,12,'"calls"',0.62999999523162842,'src/api/routes.ts','{"source":"ts-typescript . . . src/api/routes/register().","target":"ts-typescript . . . src/core/runs/listRuns().","kind":"calls","confidence":0.63,"provenance":"import_map","resolved_by":"import-map-resolver","evidence_count":0,"location":{"file":"src/api/routes.ts","span":{"start_byte":220,"end_byte":230,"start_line":7,"start_col":25,"end_line":7,"end_col":35}},"metadata":{"via":"import-map"}}',0);
INSERT INTO "edges" VALUES(1,53,'"contains"',1.0,'src/core/launch.ts','{"source":"file . src/core/launch.ts:","target":"value synthetic ts-typescript . . . src/core/launch/startRun().:local:posture:","kind":"contains","confidence":1.0,"provenance":"parsed","resolved_by":"call-value-flow","evidence_count":0,"location":{"file":"src/core/launch.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',0);
INSERT INTO "edges" VALUES(18,4,'{"other":"flows_to"}',0.62999999523162842,'src/core/launch.ts','{"source":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:param:spec:","target":"value synthetic ts-typescript . . . src/core/launch/startRun().:param:spec:","kind":{"other":"flows_to"},"confidence":0.63,"provenance":"import_map","resolved_by":"import-map-resolver","evidence_count":0,"location":{"file":"src/core/launch.ts","span":{"start_byte":123,"end_byte":142,"start_line":3,"start_col":18,"end_line":3,"end_col":37}},"metadata":{"construct":"call_argument","constructs":["call_argument"],"flow_evidence":["call_derived"],"flow_rules":["engine/call_derived/call_argument"],"flow_semantics":["value_preserving"],"flow_support":[{"confidence":0.63,"construct":"call_argument","end_byte":142,"evidence":"call_derived","file":"src/core/launch.ts","line":3,"provenance":"import_map","resolved_by":"import-map-resolver","rule":"engine/call_derived/call_argument","semantics":"value_preserving","start_byte":123}]}}',0);
INSERT INTO "edges" VALUES(53,19,'{"other":"flows_to"}',0.62999999523162842,'src/core/launch.ts','{"source":"value synthetic ts-typescript . . . src/core/launch/startRun().:local:posture:","target":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:return:value:","kind":{"other":"flows_to"},"confidence":0.63,"provenance":"import_map","resolved_by":"import-map-resolver","evidence_count":0,"location":{"file":"src/core/launch.ts","span":{"start_byte":123,"end_byte":142,"start_line":3,"start_col":18,"end_line":3,"end_col":37}},"metadata":{"construct":"call_result","constructs":["call_result"],"flow_evidence":["call_derived"],"flow_rules":["engine/call_derived/call_result"],"flow_semantics":["value_preserving"],"flow_support":[{"confidence":0.63,"construct":"call_result","end_byte":142,"evidence":"call_derived","file":"src/core/launch.ts","line":3,"provenance":"import_map","resolved_by":"import-map-resolver","rule":"engine/call_derived/call_result","semantics":"value_preserving","start_byte":123}]}}',0);
CREATE TABLE embeddings (
  symbol TEXT PRIMARY KEY,  -- the string SymbolId (resolves via symbols.sym → symbols.sid → nodes.symbol)
  dim    INTEGER NOT NULL,  -- embedding dimensionality
  vec    BLOB    NOT NULL   -- little-endian f32 bytes, length = dim * 4
);
CREATE TABLE file_call_refs (
  path TEXT PRIMARY KEY,
  refs TEXT NOT NULL
);
INSERT INTO "file_call_refs" VALUES('src/core/launch.ts','[{"from":"ts-typescript . . . src/core/launch/startRun().","raw_name":"launchPosture","kind":"calls","location":{"file":"src/core/launch.ts","span":{"start_byte":123,"end_byte":142,"start_line":3,"start_col":18,"end_line":3,"end_col":37}},"hints":{"imports":{"launchPosture":"./launch-posture"},"value_flow":{"args":[{"kind":"parameter","name":"spec","slot":0,"symbol":"value synthetic ts-typescript . . . src/core/launch/startRun().:param:spec:"}],"result":{"kind":"local","name":"posture","symbol":"value synthetic ts-typescript . . . src/core/launch/startRun().:local:posture:"}}}}]');
INSERT INTO "file_call_refs" VALUES('src/core/runs.ts','[]');
INSERT INTO "file_call_refs" VALUES('src/core/launch-posture.ts','[]');
INSERT INTO "file_call_refs" VALUES('src/api/routes.ts','[]');
INSERT INTO "file_call_refs" VALUES('tests/launch-posture.test.ts','[]');
CREATE TABLE files (
  path    TEXT PRIMARY KEY,
  digest  TEXT NOT NULL DEFAULT '',
  git_sha TEXT
);
INSERT INTO "files" VALUES('src/core/launch.ts','ceaee91f563603f8','1d25949e2425573d78b910097027710d6e1f4260');
INSERT INTO "files" VALUES('src/core/runs.ts','a3872bdac7d24341','2b5cf91f17b09c1bc34fd8cbd5f043c502264322');
INSERT INTO "files" VALUES('src/core/launch-posture.ts','253b3c1154495b29','5f0c4344b12b2607730b3d0118cbe1260d3d7d68');
INSERT INTO "files" VALUES('src/api/routes.ts','522eba247b8c5b5e','29d850223e0a27728aa1dc423403d800711abbfb');
INSERT INTO "files" VALUES('tests/launch-posture.test.ts','a57362323261626c','87b437d06112439cc4140957917bf0883bd0d5c3');
CREATE TABLE meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);
INSERT INTO "meta" VALUES('graph_version','1');
INSERT INTO "meta" VALUES('redaction_epoch','1');
INSERT INTO "meta" VALUES('text_redaction_epoch','1');
INSERT INTO "meta" VALUES('fts_rowid','sid');
INSERT INTO "meta" VALUES('indexing','');
INSERT INTO "meta" VALUES('indexed_root','/fixture/mini-ts');
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
INSERT INTO "node_files" VALUES(1,'src/core/launch.ts',1,'{"symbol":"file . src/core/launch.ts:","kind":"file","name":"src/core/launch.ts","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}');
INSERT INTO "node_files" VALUES(2,'src/core/launch.ts',1,'{"symbol":"ts-typescript . . . src/core/launch/startRun().","kind":"function","name":"startRun","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":58,"end_byte":182,"start_line":2,"start_col":7,"end_line":5,"end_col":1}},"signature":"function startRun(spec: { power?: boolean }) {","metadata":{"behaviour":"real","has_body":true,"value_params":["spec"]}}');
INSERT INTO "node_files" VALUES(3,'src/core/launch.ts',1,'{"symbol":"ts-typescript . . . import/./launch-posture/","kind":"import","name":"./launch-posture","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":30,"end_byte":48,"start_line":0,"start_col":30,"end_line":0,"end_col":48}},"signature":"./launch-posture","metadata":{}}');
INSERT INTO "node_files" VALUES(4,'src/core/launch.ts',1,'{"symbol":"value synthetic ts-typescript . . . src/core/launch/startRun().:param:spec:","kind":"parameter","name":"spec","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":76,"end_byte":80,"start_line":2,"start_col":25,"end_line":2,"end_col":29}},"metadata":{"value_role":"Parameter"}}');
INSERT INTO "node_files" VALUES(11,'src/core/runs.ts',1,'{"symbol":"file . src/core/runs.ts:","kind":"file","name":"src/core/runs.ts","language":"typescript","location":{"file":"src/core/runs.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}');
INSERT INTO "node_files" VALUES(12,'src/core/runs.ts',1,'{"symbol":"ts-typescript . . . src/core/runs/listRuns().","kind":"function","name":"listRuns","language":"typescript","location":{"file":"src/core/runs.ts","span":{"start_byte":7,"end_byte":53,"start_line":0,"start_col":7,"end_line":2,"end_col":1}},"signature":"function listRuns(): string[] {","metadata":{"behaviour":"real","has_body":true}}');
INSERT INTO "node_files" VALUES(15,'src/core/launch-posture.ts',1,'{"symbol":"file . src/core/launch-posture.ts:","kind":"file","name":"src/core/launch-posture.ts","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}');
INSERT INTO "node_files" VALUES(16,'src/core/launch-posture.ts',1,'{"symbol":"ts-typescript . . . src/core/launch-posture/PHASED.","kind":"constant","name":"PHASED","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":24,"start_line":0,"start_col":0,"end_line":0,"end_col":24}},"signature":"const PHASED = ''phased'';","metadata":{}}');
INSERT INTO "node_files" VALUES(17,'src/core/launch-posture.ts',1,'{"symbol":"ts-typescript . . . src/core/launch-posture/launchPosture().","kind":"function","name":"launchPosture","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":33,"end_byte":191,"start_line":2,"start_col":7,"end_line":7,"end_col":1}},"signature":"function launchPosture(spec: { power?: boolean }): string {","metadata":{"behaviour":"real","has_body":true,"value_params":["spec"]}}');
INSERT INTO "node_files" VALUES(18,'src/core/launch-posture.ts',1,'{"symbol":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:param:spec:","kind":"parameter","name":"spec","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":56,"end_byte":60,"start_line":2,"start_col":30,"end_line":2,"end_col":34}},"metadata":{"value_role":"Parameter"}}');
INSERT INTO "node_files" VALUES(19,'src/core/launch-posture.ts',1,'{"symbol":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:return:value:","kind":"synthetic","name":"launchPosture.return","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":182,"end_byte":188,"start_line":6,"start_col":9,"end_line":6,"end_col":15}},"metadata":{"value_role":"Return"}}');
INSERT INTO "node_files" VALUES(20,'src/core/launch-posture.ts',1,'{"symbol":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:local:PHASED:","kind":"variable","name":"PHASED","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":182,"end_byte":188,"start_line":6,"start_col":9,"end_line":6,"end_col":15}},"metadata":{"value_role":"Local"}}');
INSERT INTO "node_files" VALUES(33,'src/api/routes.ts',1,'{"symbol":"file . src/api/routes.ts:","kind":"file","name":"src/api/routes.ts","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}');
INSERT INTO "node_files" VALUES(34,'src/api/routes.ts',1,'{"symbol":"ts-typescript . . . src/api/routes/register().","kind":"function","name":"register","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":92,"end_byte":234,"start_line":3,"start_col":7,"end_line":8,"end_col":1}},"signature":"function register(app: any) {","metadata":{"behaviour":"real","has_body":true,"value_params":["app"]}}');
INSERT INTO "node_files" VALUES(35,'src/api/routes.ts',1,'{"symbol":"ts-typescript . . . import/../core/launch/","kind":"import","name":"../core/launch","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":25,"end_byte":41,"start_line":0,"start_col":25,"end_line":0,"end_col":41}},"signature":"../core/launch","metadata":{}}');
INSERT INTO "node_files" VALUES(36,'src/api/routes.ts',1,'{"symbol":"ts-typescript . . . import/../core/runs/","kind":"import","name":"../core/runs","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":68,"end_byte":82,"start_line":1,"start_col":25,"end_line":1,"end_col":39}},"signature":"../core/runs","metadata":{}}');
INSERT INTO "node_files" VALUES(37,'src/api/routes.ts',1,'{"symbol":"value synthetic ts-typescript . . . src/api/routes/register().:param:app:","kind":"parameter","name":"app","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":110,"end_byte":113,"start_line":3,"start_col":25,"end_line":3,"end_col":28}},"metadata":{"value_role":"Parameter"}}');
INSERT INTO "node_files" VALUES(46,'tests/launch-posture.test.ts',1,'{"symbol":"file . tests/launch-posture.test.ts:","kind":"file","name":"tests/launch-posture.test.ts","language":"typescript","location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}');
INSERT INTO "node_files" VALUES(47,'tests/launch-posture.test.ts',1,'{"symbol":"ts-typescript . . . tests/launch-posture.test/launchPostureCase().","kind":"function","name":"launchPostureCase","language":"typescript","location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":68,"end_byte":141,"start_line":2,"start_col":7,"end_line":4,"end_col":1}},"signature":"function launchPostureCase() {","metadata":{"behaviour":"test_support","behaviour_reason":"test_dir","has_body":true}}');
INSERT INTO "node_files" VALUES(48,'tests/launch-posture.test.ts',1,'{"symbol":"ts-typescript . . . import/../src/core/launch-posture/","kind":"import","name":"../src/core/launch-posture","language":"typescript","location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":30,"end_byte":58,"start_line":0,"start_col":30,"end_line":0,"end_col":58}},"signature":"../src/core/launch-posture","metadata":{}}');
INSERT INTO "node_files" VALUES(53,'src/core/launch.ts',1,'{"symbol":"value synthetic ts-typescript . . . src/core/launch/startRun().:local:posture:","kind":"variable","name":"posture","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":123,"end_byte":142,"start_line":3,"start_col":18,"end_line":3,"end_col":37}},"metadata":{"value_role":"Local"}}');
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
INSERT INTO "nodes" VALUES(1,'src/core/launch.ts','"file"','typescript','src/core/launch.ts','{"symbol":"file . src/core/launch.ts:","kind":"file","name":"src/core/launch.ts","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(2,'startRun','"function"','typescript','src/core/launch.ts','{"symbol":"ts-typescript . . . src/core/launch/startRun().","kind":"function","name":"startRun","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":58,"end_byte":182,"start_line":2,"start_col":7,"end_line":5,"end_col":1}},"signature":"function startRun(spec: { power?: boolean }) {","metadata":{"behaviour":"real","has_body":true,"value_params":["spec"]}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(3,'./launch-posture','"import"','typescript','src/core/launch.ts','{"symbol":"ts-typescript . . . import/./launch-posture/","kind":"import","name":"./launch-posture","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":30,"end_byte":48,"start_line":0,"start_col":30,"end_line":0,"end_col":48}},"signature":"./launch-posture","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(4,'spec','"parameter"','typescript','src/core/launch.ts','{"symbol":"value synthetic ts-typescript . . . src/core/launch/startRun().:param:spec:","kind":"parameter","name":"spec","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":76,"end_byte":80,"start_line":2,"start_col":25,"end_line":2,"end_col":29}},"metadata":{"value_role":"Parameter"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(11,'src/core/runs.ts','"file"','typescript','src/core/runs.ts','{"symbol":"file . src/core/runs.ts:","kind":"file","name":"src/core/runs.ts","language":"typescript","location":{"file":"src/core/runs.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(12,'listRuns','"function"','typescript','src/core/runs.ts','{"symbol":"ts-typescript . . . src/core/runs/listRuns().","kind":"function","name":"listRuns","language":"typescript","location":{"file":"src/core/runs.ts","span":{"start_byte":7,"end_byte":53,"start_line":0,"start_col":7,"end_line":2,"end_col":1}},"signature":"function listRuns(): string[] {","metadata":{"behaviour":"real","has_body":true}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(15,'src/core/launch-posture.ts','"file"','typescript','src/core/launch-posture.ts','{"symbol":"file . src/core/launch-posture.ts:","kind":"file","name":"src/core/launch-posture.ts","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(16,'PHASED','"constant"','typescript','src/core/launch-posture.ts','{"symbol":"ts-typescript . . . src/core/launch-posture/PHASED.","kind":"constant","name":"PHASED","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":0,"end_byte":24,"start_line":0,"start_col":0,"end_line":0,"end_col":24}},"signature":"const PHASED = ''phased'';","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(17,'launchPosture','"function"','typescript','src/core/launch-posture.ts','{"symbol":"ts-typescript . . . src/core/launch-posture/launchPosture().","kind":"function","name":"launchPosture","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":33,"end_byte":191,"start_line":2,"start_col":7,"end_line":7,"end_col":1}},"signature":"function launchPosture(spec: { power?: boolean }): string {","metadata":{"behaviour":"real","has_body":true,"value_params":["spec"]}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(18,'spec','"parameter"','typescript','src/core/launch-posture.ts','{"symbol":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:param:spec:","kind":"parameter","name":"spec","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":56,"end_byte":60,"start_line":2,"start_col":30,"end_line":2,"end_col":34}},"metadata":{"value_role":"Parameter"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(19,'launchPosture.return','"synthetic"','typescript','src/core/launch-posture.ts','{"symbol":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:return:value:","kind":"synthetic","name":"launchPosture.return","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":182,"end_byte":188,"start_line":6,"start_col":9,"end_line":6,"end_col":15}},"metadata":{"value_role":"Return"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(20,'PHASED','"variable"','typescript','src/core/launch-posture.ts','{"symbol":"value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:local:PHASED:","kind":"variable","name":"PHASED","language":"typescript","location":{"file":"src/core/launch-posture.ts","span":{"start_byte":182,"end_byte":188,"start_line":6,"start_col":9,"end_line":6,"end_col":15}},"metadata":{"value_role":"Local"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(33,'src/api/routes.ts','"file"','typescript','src/api/routes.ts','{"symbol":"file . src/api/routes.ts:","kind":"file","name":"src/api/routes.ts","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(34,'register','"function"','typescript','src/api/routes.ts','{"symbol":"ts-typescript . . . src/api/routes/register().","kind":"function","name":"register","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":92,"end_byte":234,"start_line":3,"start_col":7,"end_line":8,"end_col":1}},"signature":"function register(app: any) {","metadata":{"behaviour":"real","has_body":true,"value_params":["app"]}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(35,'../core/launch','"import"','typescript','src/api/routes.ts','{"symbol":"ts-typescript . . . import/../core/launch/","kind":"import","name":"../core/launch","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":25,"end_byte":41,"start_line":0,"start_col":25,"end_line":0,"end_col":41}},"signature":"../core/launch","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(36,'../core/runs','"import"','typescript','src/api/routes.ts','{"symbol":"ts-typescript . . . import/../core/runs/","kind":"import","name":"../core/runs","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":68,"end_byte":82,"start_line":1,"start_col":25,"end_line":1,"end_col":39}},"signature":"../core/runs","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(37,'app','"parameter"','typescript','src/api/routes.ts','{"symbol":"value synthetic ts-typescript . . . src/api/routes/register().:param:app:","kind":"parameter","name":"app","language":"typescript","location":{"file":"src/api/routes.ts","span":{"start_byte":110,"end_byte":113,"start_line":3,"start_col":25,"end_line":3,"end_col":28}},"metadata":{"value_role":"Parameter"}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(46,'tests/launch-posture.test.ts','"file"','typescript','tests/launch-posture.test.ts','{"symbol":"file . tests/launch-posture.test.ts:","kind":"file","name":"tests/launch-posture.test.ts","language":"typescript","location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":0,"end_byte":0,"start_line":0,"start_col":0,"end_line":0,"end_col":0}},"metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(47,'launchPostureCase','"function"','typescript','tests/launch-posture.test.ts','{"symbol":"ts-typescript . . . tests/launch-posture.test/launchPostureCase().","kind":"function","name":"launchPostureCase","language":"typescript","location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":68,"end_byte":141,"start_line":2,"start_col":7,"end_line":4,"end_col":1}},"signature":"function launchPostureCase() {","metadata":{"behaviour":"test_support","behaviour_reason":"test_dir","has_body":true}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(48,'../src/core/launch-posture','"import"','typescript','tests/launch-posture.test.ts','{"symbol":"ts-typescript . . . import/../src/core/launch-posture/","kind":"import","name":"../src/core/launch-posture","language":"typescript","location":{"file":"tests/launch-posture.test.ts","span":{"start_byte":30,"end_byte":58,"start_line":0,"start_col":30,"end_line":0,"end_col":58}},"signature":"../src/core/launch-posture","metadata":{}}',NULL,NULL,0,'',NULL,NULL);
INSERT INTO "nodes" VALUES(53,'posture','"variable"','typescript','src/core/launch.ts','{"symbol":"value synthetic ts-typescript . . . src/core/launch/startRun().:local:posture:","kind":"variable","name":"posture","language":"typescript","location":{"file":"src/core/launch.ts","span":{"start_byte":123,"end_byte":142,"start_line":3,"start_col":18,"end_line":3,"end_col":37}},"metadata":{"value_role":"Local"}}',NULL,NULL,0,'',NULL,NULL);
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
INSERT INTO "nodes_fts_content" VALUES(1,'file . src/core/launch.ts:','src/core/launch.ts','','');
INSERT INTO "nodes_fts_content" VALUES(2,'ts-typescript . . . src/core/launch/startRun().','startRun','function startRun(spec: { power?: boolean }) {','');
INSERT INTO "nodes_fts_content" VALUES(3,'ts-typescript . . . import/./launch-posture/','./launch-posture','./launch-posture','');
INSERT INTO "nodes_fts_content" VALUES(4,'value synthetic ts-typescript . . . src/core/launch/startRun().:param:spec:','spec','','');
INSERT INTO "nodes_fts_content" VALUES(11,'file . src/core/runs.ts:','src/core/runs.ts','','');
INSERT INTO "nodes_fts_content" VALUES(12,'ts-typescript . . . src/core/runs/listRuns().','listRuns','function listRuns(): string[] {','');
INSERT INTO "nodes_fts_content" VALUES(15,'file . src/core/launch-posture.ts:','src/core/launch-posture.ts','','');
INSERT INTO "nodes_fts_content" VALUES(16,'ts-typescript . . . src/core/launch-posture/PHASED.','PHASED','const PHASED = ''phased'';','');
INSERT INTO "nodes_fts_content" VALUES(17,'ts-typescript . . . src/core/launch-posture/launchPosture().','launchPosture','function launchPosture(spec: { power?: boolean }): string {','');
INSERT INTO "nodes_fts_content" VALUES(18,'value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:param:spec:','spec','','');
INSERT INTO "nodes_fts_content" VALUES(19,'value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:return:value:','launchPosture.return','','');
INSERT INTO "nodes_fts_content" VALUES(20,'value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:local:PHASED:','PHASED','','');
INSERT INTO "nodes_fts_content" VALUES(33,'file . src/api/routes.ts:','src/api/routes.ts','','');
INSERT INTO "nodes_fts_content" VALUES(34,'ts-typescript . . . src/api/routes/register().','register','function register(app: any) {','');
INSERT INTO "nodes_fts_content" VALUES(35,'ts-typescript . . . import/../core/launch/','../core/launch','../core/launch','');
INSERT INTO "nodes_fts_content" VALUES(36,'ts-typescript . . . import/../core/runs/','../core/runs','../core/runs','');
INSERT INTO "nodes_fts_content" VALUES(37,'value synthetic ts-typescript . . . src/api/routes/register().:param:app:','app','','');
INSERT INTO "nodes_fts_content" VALUES(46,'file . tests/launch-posture.test.ts:','tests/launch-posture.test.ts','','');
INSERT INTO "nodes_fts_content" VALUES(47,'ts-typescript . . . tests/launch-posture.test/launchPostureCase().','launchPostureCase','function launchPostureCase() {','');
INSERT INTO "nodes_fts_content" VALUES(48,'ts-typescript . . . import/../src/core/launch-posture/','../src/core/launch-posture','../src/core/launch-posture','');
INSERT INTO "nodes_fts_content" VALUES(53,'value synthetic ts-typescript . . . src/core/launch/startRun().:local:posture:','posture','','');
CREATE TABLE 'nodes_fts_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO "nodes_fts_data" VALUES(1,X'15002D2100');
INSERT INTO "nodes_fts_data" VALUES(10,X'0000000002061500061101010101010201010301010401010501010000');
INSERT INTO "nodes_fts_data" VALUES(137438953473,X'00000032093066756E6374696F6E0C0601020201086C69737472756E730C0C0101020102030106737472696E670C06010204040F12');
INSERT INTO "nodes_fts_data" VALUES(274877906945,X'0000003A07306C61756E63682E060101030107706F73747572652E060101040104746573742E060101050501732E060101020201732E06010106040D0E0B08');
INSERT INTO "nodes_fts_data" VALUES(412316860417,X'0000002E093066756E6374696F6E2F0601020201116C61756E6368706F7374757265636173652F0C010102010203040F');
INSERT INTO "nodes_fts_data" VALUES(549755813889,X'000000400530636F7265300C01010301020301066C61756E6368300C0101040102040107706F7374757265300C0101050102050103737263300C010102010202040E1011');
INSERT INTO "nodes_fts_data" VALUES(687194767361,X'000000120830706F7374757265350601010204');
INSERT INTO "nodes_fts_data" VALUES(2336462209025,X'000001920430616E792206010205020270692106010103030170220601020403060101020107626F6F6C65616E02060102060F060102060105636F6E737410060102020302726501060101030A060101030406010103140C010102010202010C010102010202010866756E6374696F6E02060102020F06010202110601020201066C61756E63680106010104020C0101020102020C06010104140C0101030102030707706F7374757265110C01010201020302060101020106706861736564100E01010201020303040601010202066F7374757265030C0101030102030C06010105030377657202060102050F0601020501087265676973746572220C01010201020303047475726E130601010302056F7574657321060101040203756E730B06010104190C010103010203010473706563020601020402060101020D0601020401060101020202726301060101020A060101020406010102120601010202077461727472756E020C010102010203030472696E6711060102070102747301060101050A0601010504060101061206010105040A090D130C2319221616150F120B0C121A18110B');
CREATE TABLE 'nodes_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
INSERT INTO "nodes_fts_docsize" VALUES(1,X'00040000');
INSERT INTO "nodes_fts_docsize" VALUES(2,X'00010500');
INSERT INTO "nodes_fts_docsize" VALUES(3,X'00020200');
INSERT INTO "nodes_fts_docsize" VALUES(4,X'00010000');
INSERT INTO "nodes_fts_docsize" VALUES(11,X'00040000');
INSERT INTO "nodes_fts_docsize" VALUES(12,X'00010300');
INSERT INTO "nodes_fts_docsize" VALUES(15,X'00050000');
INSERT INTO "nodes_fts_docsize" VALUES(16,X'00010300');
INSERT INTO "nodes_fts_docsize" VALUES(17,X'00010600');
INSERT INTO "nodes_fts_docsize" VALUES(18,X'00010000');
INSERT INTO "nodes_fts_docsize" VALUES(19,X'00020000');
INSERT INTO "nodes_fts_docsize" VALUES(20,X'00010000');
INSERT INTO "nodes_fts_docsize" VALUES(33,X'00040000');
INSERT INTO "nodes_fts_docsize" VALUES(34,X'00010400');
INSERT INTO "nodes_fts_docsize" VALUES(35,X'00020200');
INSERT INTO "nodes_fts_docsize" VALUES(36,X'00020200');
INSERT INTO "nodes_fts_docsize" VALUES(37,X'00010000');
INSERT INTO "nodes_fts_docsize" VALUES(46,X'00050000');
INSERT INTO "nodes_fts_docsize" VALUES(47,X'00010200');
INSERT INTO "nodes_fts_docsize" VALUES(48,X'00040400');
INSERT INTO "nodes_fts_docsize" VALUES(53,X'00010000');
CREATE TABLE 'nodes_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
INSERT INTO "nodes_fts_idx" VALUES(1,X'',2);
INSERT INTO "nodes_fts_idx" VALUES(2,X'',2);
INSERT INTO "nodes_fts_idx" VALUES(3,X'',2);
INSERT INTO "nodes_fts_idx" VALUES(4,X'',2);
INSERT INTO "nodes_fts_idx" VALUES(5,X'',2);
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
INSERT INTO "symbols" VALUES(1,'file . src/core/launch.ts:',0,1);
INSERT INTO "symbols" VALUES(2,'ts-typescript . . . src/core/launch/startRun().',0,1);
INSERT INTO "symbols" VALUES(3,'ts-typescript . . . import/./launch-posture/',0,1);
INSERT INTO "symbols" VALUES(4,'value synthetic ts-typescript . . . src/core/launch/startRun().:param:spec:',0,1);
INSERT INTO "symbols" VALUES(11,'file . src/core/runs.ts:',0,1);
INSERT INTO "symbols" VALUES(12,'ts-typescript . . . src/core/runs/listRuns().',0,1);
INSERT INTO "symbols" VALUES(15,'file . src/core/launch-posture.ts:',0,1);
INSERT INTO "symbols" VALUES(16,'ts-typescript . . . src/core/launch-posture/PHASED.',0,1);
INSERT INTO "symbols" VALUES(17,'ts-typescript . . . src/core/launch-posture/launchPosture().',0,1);
INSERT INTO "symbols" VALUES(18,'value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:param:spec:',0,1);
INSERT INTO "symbols" VALUES(19,'value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:return:value:',0,1);
INSERT INTO "symbols" VALUES(20,'value synthetic ts-typescript . . . src/core/launch-posture/launchPosture().:local:PHASED:',0,1);
INSERT INTO "symbols" VALUES(33,'file . src/api/routes.ts:',0,1);
INSERT INTO "symbols" VALUES(34,'ts-typescript . . . src/api/routes/register().',0,1);
INSERT INTO "symbols" VALUES(35,'ts-typescript . . . import/../core/launch/',0,1);
INSERT INTO "symbols" VALUES(36,'ts-typescript . . . import/../core/runs/',0,1);
INSERT INTO "symbols" VALUES(37,'value synthetic ts-typescript . . . src/api/routes/register().:param:app:',0,1);
INSERT INTO "symbols" VALUES(46,'file . tests/launch-posture.test.ts:',0,1);
INSERT INTO "symbols" VALUES(47,'ts-typescript . . . tests/launch-posture.test/launchPostureCase().',0,1);
INSERT INTO "symbols" VALUES(48,'ts-typescript . . . import/../src/core/launch-posture/',0,1);
INSERT INTO "symbols" VALUES(53,'value synthetic ts-typescript . . . src/core/launch/startRun().:local:posture:',0,1);
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
INSERT INTO "unresolved_refs" VALUES(1,34,'post','"calls"','src/api/routes.ts',4,124,193);
INSERT INTO "unresolved_refs" VALUES(2,34,'get','"calls"','src/api/routes.ts',7,197,231);
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
INSERT INTO "sqlite_sequence" VALUES('symbols',77);
INSERT INTO "sqlite_sequence" VALUES('changes',5);
INSERT INTO "sqlite_sequence" VALUES('annotations',4);
COMMIT;
