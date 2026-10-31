"""Determinism matrix (design 23.4 P2-6): digest one machine's outputs, or compare machines.

Every output of the determinism set is either a pure data file (traces, snapshot images, wasm
modules) or a log that mixes deterministic results with wall-clock timings. A digest is the
SHA-256 of the file after `normalize`, which masks wall-clock (CPU) timings and where the machine
wrote its outputs, and nothing else:
  * durations in the phrases the tools print CPU timings with (`WALL`: "in 349.8ms", "step avg 0.39
    ms", "(303.9 us/frame)", ...), found by diffing two machines' raw outputs. Virtual-clock times
    (event times, queue delays, recovery times, configured latencies) are results and are compared;
  * the values after "depth:ms", and table cells under a CPU-timing header ("CPU ms ...", "worst tick");
  * cargo's progress lines, and runs of spaces (timing widths shift column padding);
  * the output directory echoed in logs (".../o/"), and sha256sum's Windows '*' marker.
Data files (traces, snapshot images, wasm modules, MCP transcripts) are hashed raw: see `is_raw`.

usage:
  python -I scripts/matrix_digest.py digest OUTDIR            writes OUTDIR/digests.txt
  python -I scripts/matrix_digest.py compare NAME=OUTDIR ...  table; exit 1 on any difference
  python -I scripts/matrix_digest.py show FILE                print FILE as normalized
"""

import hashlib
import os
import re
import sys

RAW = re.compile(r"(^|/)(trace|images)-[^/]*\.txt$|\.wasm$|\.jsonl$")
# Where each machine writes its outputs (run_matrix.sh's PE_OUT always ends in /o), as echoed in logs.
OUTDIR = re.compile(r"\S*/o/")
# sha256sum marks binary mode with '*' on Windows and prints two spaces on Linux.
SHALINE = re.compile(r"^([0-9a-f]{64}) [ *]")


def is_raw(rel):
    return bool(RAW.search(rel))

SKIP = ("digests.txt", "env.txt", "matrix.txt")
CARGO = re.compile(r"^\s*(Compiling|Finished|Running|Blocking|Downloaded|Downloading|Updating|Locking|Adding|warning|Building)\b")
# Wall-clock (CPU) timings, by the phrases the tools print them with. Only these are masked: the
# simulators' virtual-clock times (event times, queue delays, recovery times, configured latencies)
# are deterministic results and are compared (review M1). Found by diffing two machines' raw outputs.
_N = r"\d+(?:\.\d+)?(?:e-?\d+)?"
_D = _N + r" ?(?:ms|µs|us|ns|s)\b"
WALL = [re.compile(p) for p in [
    r"(\bin )" + _D,                                             # goldens, rollback, hotswap: "... in 349.8ms"
    r"(\bre-sim )" + _D,                                         # ball pit: "re-sim 3.44 ms per rendered sim frame"
    r"(\()" + _D + r"( over the window\))",                      #   "(757 ms over the window)"
    r"(\bstep avg )" + _D,
    r"(\bstep )" + _D + r"( plain, )" + _D + r"( with native hooks)",  # R15 hooks
    r"(\bresim frames \d+ \()" + _D + r"(\))",                   # session-sim member stats
    r"\b" + _D + r"( total \()" + _N + r"( us/frame\))",          # physics trace
    r"(\btotal )" + _D,                                          # rollback T3
    r"(\()" + _N + r"( us/frame(?: interpreted)?\))",             # rollback, hotswap
    r"(module validate\+compile: v1 )" + _D + r"(, v2 )" + _D,   # hotswap
    r"(module compile )" + _D,
    r"(\bswap step )" + _D,
    r"(\bwall )" + _D,                                           # view-change scale run
    r"(\bsave )" + _D + r"(, restore )" + _D,                    # rollback, perturbation history
    r"(\bdiff scan )" + _D,
    r"(\bstep )" + _D,                                           # rollback "step 4.18ms", hotswap "swap step"
    r"(per-frame snapshots: )" + _D,                             # hotswap
    r"(\bafter )" + _D,                                          # hotswap D3: "trap ... after 267µs"
    r"(\bsim CPU )" + _D,                                        # perturbation-history observer
    r"(\bworst tick )" + _D,
]]
# Inside a matched phrase, only the durations themselves (and the count before "us/frame") become T.
_UNIT = re.compile(_D)
_PERFRAME = re.compile(_N + r"(?= us/frame)")
# Table columns that hold CPU timings (perturbation history: "CPU ms per frame", "worst tick ms").
TIMECOL = re.compile(r"\b(CPU|worst tick)\b")


def normalize_lines(text):
    out = []
    header = None  # (number of cells, mask per cell) of the last table header seen
    for line in text.splitlines():
        if CARGO.match(line):
            continue
        if "depth:ms" in line:
            line = re.sub(r"(\d+):\d+(?:\.\d+)?", r"\1:T", line)
        cells = line.split("|")
        if len(cells) >= 3:
            if not re.search(r"\d", "".join(cells[1:])) or any(TIMECOL.search(c) for c in cells[1:]) and \
                    sum(bool(re.search(r"[A-Za-z]{3}", c)) for c in cells[1:]) >= len(cells) // 2:
                header = (len(cells), [bool(TIMECOL.search(c)) for c in cells])
            elif header and header[0] == len(cells):
                cells = ["T" if m else c for c, m in zip(cells, header[1])]
                line = "|".join(cells)
        line = SHALINE.sub(r"\1 ", OUTDIR.sub("<out>/", line))
        for w in WALL:
            line = w.sub(lambda m: _UNIT.sub("T", _PERFRAME.sub("T", m.group(0))), line)
        out.append(" ".join(line.split()))
    return out


def file_digest(path):
    data = open(path, "rb").read()
    if is_raw(path.replace(os.sep, "/")):
        return hashlib.sha256(data).hexdigest()[:16], "raw"
    text = data.decode("utf-8", "replace")
    norm = "\n".join(normalize_lines(text)).encode()
    return hashlib.sha256(norm).hexdigest()[:16], "norm"


def walk(outdir):
    for root, _, files in os.walk(outdir):
        for f in files:
            p = os.path.join(root, f)
            rel = os.path.relpath(p, outdir).replace(os.sep, "/")
            if rel in SKIP:
                continue
            yield rel, p


def digest(outdir):
    rows = sorted((rel, *file_digest(p)) for rel, p in walk(outdir))
    with open(os.path.join(outdir, "digests.txt"), "w", encoding="utf-8", newline="\n") as f:
        for rel, h, kind in rows:
            f.write(f"{h} {kind} {rel}\n")
    print(f"{len(rows)} outputs digested -> {outdir}/digests.txt")


def load(outdir):
    d = {}
    for line in open(os.path.join(outdir, "digests.txt"), encoding="utf-8"):
        h, kind, rel = line.rstrip("\n").split(" ", 2)
        d[rel] = (h, kind)
    return d


def first_diff(a, b):
    """The first differing line of two outputs, as normalized (or the first byte for raw files)."""
    if is_raw(a.replace(os.sep, "/")):
        x, y = open(a, "rb").read(), open(b, "rb").read()
        i = next((i for i in range(min(len(x), len(y))) if x[i] != y[i]), min(len(x), len(y)))
        lx = x[:i].count(b"\n") + 1
        return f"first differing byte {i} (line {lx}); sizes {len(x)} / {len(y)}"
    la = normalize_lines(open(a, encoding="utf-8", errors="replace").read())
    lb = normalize_lines(open(b, encoding="utf-8", errors="replace").read())
    for i, (x, y) in enumerate(zip(la, lb)):
        if x != y:
            return f"normalized line {i + 1}:\n      {x[:200]}\n      {y[:200]}"
    return f"one output is a prefix of the other ({len(la)} / {len(lb)} lines)"


def compare(specs):
    names = [s.split("=", 1)[0] for s in specs]
    dirs = [s.split("=", 1)[1] for s in specs]
    tables = [load(d) for d in dirs]
    files = sorted(set().union(*tables))
    ref = tables[0]
    w = max(len(f) for f in files)
    print(f"{'output':<{w}}  " + "  ".join(f"{n:<16}" for n in names) + "  verdict")
    bad = []
    for f in files:
        cells = [t.get(f, ("missing", ""))[0] for t in tables]
        same = len(set(cells)) == 1 and cells[0] != "missing"
        print(f"{f:<{w}}  " + "  ".join(f"{c:<16}" for c in cells) + ("  equal" if same else "  DIFFER"))
        if not same:
            bad.append(f)
    print()
    if not bad:
        print(f"MATRIX: PASS, {len(files)} outputs equal on {len(names)} machines ({', '.join(names)})")
        return 0
    f = bad[0]
    print(f"MATRIX: FAIL, {len(bad)} of {len(files)} outputs differ; first: {f}")
    for n, d, t in zip(names[1:], dirs[1:], tables[1:]):
        if t.get(f) != ref.get(f):
            if f in t and f in ref:
                print(f"  {names[0]} vs {n}: {first_diff(os.path.join(dirs[0], f), os.path.join(d, f))}")
            else:
                print(f"  {names[0]} vs {n}: missing on {names[0] if f not in ref else n}")
    return 1


def main():
    if len(sys.argv) >= 3 and sys.argv[1] == "digest":
        digest(sys.argv[2])
    elif len(sys.argv) >= 3 and sys.argv[1] == "compare":
        sys.exit(compare(sys.argv[2:]))
    elif len(sys.argv) == 3 and sys.argv[1] == "show":
        print("\n".join(normalize_lines(open(sys.argv[2], encoding="utf-8", errors="replace").read())))
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main()
