"""Real-shape fixture for C2: garden's real domain-run route on its mini-cobol tree, written by the real
`wicked-estate write-batch`, dumped as SQL. Seat answers are fakes in the writer's and checker's output shapes
(garden tests/domain/test_v3_store.py:138-184 FakeSeats, plus one statement whose trigger claim the checker
cannot confirm, so the run has an open statement and a follow-up row). Usage: gen.py <garden> <work> <out.sql>"""
import json, os, re, shutil, sqlite3, subprocess, sys

garden, work, out = sys.argv[1:4]
sys.path.insert(0, os.path.join(garden, "scripts"))
from domain import graph, model_calls, run, scope  # noqa: E402

PROG = "app/cbl/PAYCALC1.cbl"
tree = os.path.join(work, "mini-cobol")
db = os.path.join(work, "real.db")
shutil.rmtree(work, ignore_errors=True)
os.makedirs(work)
shutil.copytree(os.path.join(garden, "tests/domain/fixtures/v3/mini-cobol"), tree)
env = dict(os.environ, HOME=os.path.join(work, "home"), WICKED_HOME=os.path.join(work, "wh"))
subprocess.run(["wicked-estate", "index", tree, "--db", db], check=True, capture_output=True, env=env)
os.environ.update(HOME=env["HOME"], WICKED_HOME=env["WICKED_HOME"])
repo = graph.Repo(tree, db)
S = scope.build_scope(repo, "PAYCALC1")


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
                if t.get("name") == "READ-PARA":
                    out.append(dict(target=t["id"], not_rules=[], statements=[
                        dict(anchor=dict(file=PROG, lines=t["lines"]), claims=[
                            dict(i=0, type="condition", text="`PAY-HOURS > 40`", lines=[f"{PROG}:24"]),
                            dict(i=1, type="effect", text="`PAY-AMOUNT` gets the overtime rate", when=[0], lines=[f"{PROG}:25-26"]),
                            dict(i=2, type="else", text="`PAY-AMOUNT` is `PAY-RATE * PAY-HOURS`", lines=[f"{PROG}:27-28"])]),
                        dict(anchor=dict(file=PROG, lines=t["lines"]), claims=[
                            dict(i=0, type="condition", text="the read reaches the end of `PAY-FILE`", lines=[f"{PROG}:21-22"]),
                            dict(i=1, type="effect", text="`WS-EOF` is set to `'Y'`", when=[0], lines=[f"{PROG}:22"])]),
                        dict(anchor=dict(file=PROG, lines=t["lines"]), claims=[
                            dict(i=0, type="trigger", text="performed until `WS-EOF = 'Y'`", lines=[f"{PROG}:17"]),
                            dict(i=1, type="effect", text="`PAY-RATE` is read", when=[0], lines=[f"{PROG}:25-26"])]),
                        dict(anchor=dict(file=PROG, lines=t["lines"]), claims=[
                            dict(i=0, type="condition", text="`PAY-HOURS > 40`", lines=[f"{PROG}:24"]),
                            dict(i=1, type="effect", text="`PAY-AMOUNT` is set to zero", when=[0], lines=[f"{PROG}:25-26"])])]))
                else:
                    a, b = t["lines"]
                    out.append(dict(target=t["id"], statements=[], not_rules=[dict(lines=f"{t['file']}:{a}-{b}", reason="control-only")]))
            return json.dumps(dict(targets=out)), 0
        if step == "prose":
            sids = sorted(set(re.findall(r"^\[(S\d+)\]$", prompt, re.M)), key=lambda s: int(s[1:]))
            return json.dumps(dict(groups=[dict(g=1, title="Overtime pay", area="PAYCALC1", kind="business",
                                                rule=dict(text="Hours over forty are paid at the overtime rate.", cites=sids),
                                                details=[dict(text="Every overtime hour is paid double.", cites=sids)])])), 0
        return "{}", 0

    def codex(self, step, cid, prompt, timeout=1500, role=None, **kw):
        self.n += 1
        if step.startswith("check") or step.startswith("recheck"):
            res = []
            for ln in prompt.splitlines():
                if ln.startswith('{"n":'):
                    it = json.loads(ln)
                    cl = []
                    for c in it["claims"]:
                        if "reaches the end" in json.dumps(c):
                            cl.append(dict(i=c["i"], verdict="UNVERIFIABLE", reason="the excerpt does not show what AT END covers",
                                           need="the FD of PAY-FILE"))
                        elif "set to zero" in json.dumps(c):
                            cl.append(dict(i=c["i"], verdict="OVERREACH", case="VALUE",
                                           witness="`PAY-AMOUNT` is computed from the overtime rate, never zero (line 25)"))
                        elif "performed until" in json.dumps(c):
                            cl.append(dict(i=c["i"], verdict="UNVERIFIABLE", reason="the loop's caller is outside the excerpt",
                                           need="the paragraph that PERFORMs READ-PARA"))
                        else:
                            cl.append(dict(i=c["i"], verdict="SUPPORTED"))
                    res.append(dict(n=it["n"], claims=cl))
            return json.dumps(dict(results=res)), 0
        if step.startswith("prose"):
            ids = re.findall(r"^\[(g\d+\.\w+)\]", prompt, re.M)
            return json.dumps(dict(verdicts=[dict(id=i, verdict="OVERREACH", reason="QUANTIFIER: the statements give no multiplier")
                                             if ".d" in i else dict(id=i, verdict="SUPPORTED") for i in ids])), 0
        return "{}", 0


seats = Seats()
model_calls.claude, model_calls.codex = seats.claude, seats.codex
res, _, groups, _ = run.run(tree, db, dict(intent="PAYCALC1", description="payroll"), os.path.join(work, "run1"), log=lambda s: None)
print(json.dumps(dict(calls=seats.n, checked=res.get("checked"), open=res.get("open"), store=res.get("store"),
                      unplaced=res.get("unplaced")), default=str))
con = sqlite3.connect(db)
kinds = con.execute("select type, count(*) from annotations group by type order by type").fetchall()
print("annotations:", kinds)
# the one machine-specific value in the store: the index's absolute root
con.execute("update meta set v = ? where k = 'indexed_root'", ("/fixture/mini-cobol",))
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
