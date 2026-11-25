"""Real-shape fixture for C2: garden's real domain-run route on its mini-ts tree, written by the real
`wicked-estate write-batch`, dumped as SQL. Seat answers are fakes in the writer's and checker's output shapes
(garden tests/domain/test_v3_store.py:138-184 FakeSeats, a TypeScript statement, and a housekeeping group
numbered before the business group so a reader must order it last). Usage: gen.py <garden> <work> <out.sql>"""
import json, os, re, shutil, sqlite3, subprocess, sys

garden, work, out = sys.argv[1:4]
sys.path.insert(0, os.path.join(garden, "scripts"))
from domain import graph, model_calls, run, scope  # noqa: E402

PROG = "src/core/launch-posture.ts"
tree = os.path.join(work, "mini-ts")
db = os.path.join(work, "real.db")
shutil.rmtree(work, ignore_errors=True)
os.makedirs(work)
shutil.copytree(os.path.join(garden, "tests/domain/fixtures/v3/mini-ts"), tree)
env = dict(os.environ, HOME=os.path.join(work, "home"), WICKED_HOME=os.path.join(work, "wh"))
subprocess.run(["wicked-estate", "index", tree, "--db", db], check=True, capture_output=True, env=env)
os.environ.update(HOME=env["HOME"], WICKED_HOME=env["WICKED_HOME"])
repo = graph.Repo(tree, db)
S = scope.build_scope(repo, "launch posture", ["launch posture"])


class Seats:
    def __init__(self):
        self.n = 0

    def claude(self, step, cid, prompt, timeout=1500, role=None, **kw):
        self.n += 1
        if step == "extract":
            out = []
            for t in S["targets"]:
                if f'"target": "{t["id"]}"' not in prompt:
                    continue
                out.append(dict(target=t["id"], not_rules=[], statements=[dict(anchor=dict(file=PROG, lines=t["lines"]), claims=[
                    dict(i=0, type="condition", text="`spec.power && process.env.WICKED_POWER === '1'`", lines=[f"{PROG}:4"]),
                    dict(i=1, type="effect", text="returns `'power'`", when=[0], lines=[f"{PROG}:5"]),
                    dict(i=2, type="else", text="returns `PHASED`", lines=[f"{PROG}:7"])])]))
            return json.dumps(dict(targets=out)), 0
        if step == "prose":
            sids = sorted(set(re.findall(r"^\[(S\d+)\]$", prompt, re.M)), key=lambda s: int(s[1:]))
            return json.dumps(dict(groups=[
                dict(g=1, title="Posture fallback", area="launch-posture", kind="housekeeping",
                     rule=dict(text="A launch that is not a power launch runs phased.", cites=sids), details=[]),
                dict(g=2, title="Power launches", area="launch-posture", kind="business",
                     rule=dict(text="A launch runs in power mode only when power is asked for and enabled.", cites=sids),
                     details=[])])), 0
        return "{}", 0

    def codex(self, step, cid, prompt, timeout=1500, role=None, **kw):
        self.n += 1
        if step.startswith("check") or step.startswith("recheck"):
            res = []
            for ln in prompt.splitlines():
                if ln.startswith('{"n":'):
                    it = json.loads(ln)
                    res.append(dict(n=it["n"], claims=[dict(i=c["i"], verdict="SUPPORTED") for c in it["claims"]]))
            return json.dumps(dict(results=res)), 0
        if step.startswith("prose"):
            ids = re.findall(r"^\[(g\d+\.\w+)\]", prompt, re.M)
            return json.dumps(dict(verdicts=[dict(id=i, verdict="SUPPORTED") for i in ids])), 0
        return "{}", 0


seats = Seats()
model_calls.claude, model_calls.codex = seats.claude, seats.codex
res, _, groups, _ = run.run(tree, db, dict(intent="launch posture", phrases=["launch posture"], description="how a launch picks its posture"), os.path.join(work, "run1"), log=lambda s: None)
print(json.dumps(dict(calls=seats.n, checked=res.get("checked"), open=res.get("open"), store=res.get("store"),
                      unplaced=res.get("unplaced")), default=str))
con = sqlite3.connect(db)
kinds = con.execute("select type, count(*) from annotations group by type order by type").fetchall()
print("annotations:", kinds)
# the one machine-specific value in the store: the index's absolute root
con.execute("update meta set v = ? where k = 'indexed_root'", ("/fixture/mini-ts",))
con.commit()
import hashlib
def _sha(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()
garden_commit = subprocess.run(["git", "-C", garden, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
estate = subprocess.run(["wicked-estate", "--version"], capture_output=True, text=True).stdout.strip()
tree_files = sorted(os.path.relpath(os.path.join(d, f), tree) for d, _, fs in os.walk(tree) for f in fs)
with open(out, "w") as fh:
    fh.write(f"-- J6 C2 real-shape fixture: the estate store after garden's domain-run wrote one run (DES-C2-v2 F27, R23).\n")
    fh.write(f"-- Writer: garden {garden_commit} scripts/domain/store.py rows_for (:303-381) through {estate} write-batch.\n")
    fh.write(f"-- Generator: tests/fixtures/domain_v3/gen/{os.path.basename(__file__)} sha256 {_sha(__file__)}; seat answers are scenario fakes.\n")
    for f in tree_files:
        fh.write(f"-- tree {f} sha256 {_sha(os.path.join(tree, f))}\n")
    fh.write("-- Regenerate: python3 <generator> <garden checkout> <scratch dir> <this file>; meta.indexed_root is neutralized.\n")
    for line in con.iterdump():
        # An FTS5 table's rows live in its shadow tables (dumped too); its own INSERTs fail on a
        # fresh load because the dump only registers the table through writable_schema.
        if line.startswith('INSERT INTO "nodes_fts" VALUES'):
            continue
        fh.write(line + "\n")
con.close()
