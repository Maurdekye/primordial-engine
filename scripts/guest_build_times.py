"""P2-7 build-time table for the activation lead (design 13.2, 23.4): how long a peer takes to build a
committed guest module from source, with the reproducible-build flags of scripts/build_wasm_guests.sh.

For each module: a cold build (a fresh target directory, as when joining a world), a no-op rebuild, and
rebuilds after a one-line edit (as when a commit activates), the edit undone in between. Timings are
wall clock, so run it on an otherwise idle machine; under the machine-load rule they are noisy.

usage (from the repo root): python -I scripts/guest_build_times.py [--reps N] [--label HOST]
"""

import os
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
W = "--target wasm32-unknown-unknown"
# (module, workspace, cargo arguments, edited file, original line, edited line)
MODULES = [
    ("logic_v2 (spike guest, 1.1 KB)", "spikes/wasm-guests", f"build -q --release {W} -p logic_v2",
     "spikes/wasm-guests/logic-v2/src/lib.rs", "s.spin * 1.07 + 0.01", "s.spin * 1.08 + 0.01"),
    ("physics_in_wasm (Box3D, 0.5 MB)", ".", f"build -q --profile wasm-guest {W} -p physics_in_wasm",
     None, None, None),
    ("logic_mono (6.4k lines, one crate)", "spikes/iter-bench/guests", f"build -q --release {W} -p logic_mono",
     "spikes/iter-bench/guests/logic_mono/src/sys_movement.rs",
     "let friction = 0.980; // ITER-EDIT", "let friction = 0.981; // ITER-EDIT"),
    ("logic_wasm (6.4k lines, 9 crates), leaf edit", "spikes/iter-bench/guests", f"build -q --release {W} -p logic_wasm",
     "spikes/iter-bench/guests/sys_movement/src/lib.rs",
     "let friction = 0.980; // ITER-EDIT", "let friction = 0.981; // ITER-EDIT"),
    ("logic_wasm, core edit (every crate rebuilds)", "spikes/iter-bench/guests", f"build -q --release {W} -p logic_wasm",
     "spikes/iter-bench/guests/pe_api/src/math.rs",
     "let k = 3.0; // ITER-EDIT-CORE", "let k = 3.001; // ITER-EDIT-CORE"),
]


def longpath(p):
    try:
        return subprocess.run(["cygpath", "-w", "-l", p], capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return p


def env_for(target_dir):
    e = dict(os.environ)
    home = longpath(os.environ.get("CARGO_HOME") or os.path.join(os.path.expanduser("~"), ".cargo"))
    e["CARGO_ENCODED_RUSTFLAGS"] = f"--remap-path-prefix={longpath(ROOT)}=/pe\x1f--remap-path-prefix={home}=/cargo"
    e["CARGO_TARGET_DIR"] = target_dir
    return e


def build(ws, args, env):
    t = time.perf_counter()
    r = subprocess.run(["cargo", *args.split()], cwd=os.path.join(ROOT, ws), env=env, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"build failed in {ws}: {r.stderr[-2000:]}")
    return time.perf_counter() - t


def edit(path, a, b):
    p = os.path.join(ROOT, path)
    s = open(p, encoding="utf-8").read()
    assert a in s, (path, a)
    open(p, "w", encoding="utf-8", newline="\n").write(s.replace(a, b))


def main():
    reps = int(sys.argv[sys.argv.index("--reps") + 1]) if "--reps" in sys.argv else 3
    label = sys.argv[sys.argv.index("--label") + 1] if "--label" in sys.argv else os.environ.get("COMPUTERNAME", "")
    rustc = subprocess.run(["rustc", "-V"], capture_output=True, text=True).stdout.strip()
    jobs = os.environ.get("CARGO_BUILD_JOBS", "all cores")
    print(f"RESULT guest build times on {label} ({rustc}; {os.cpu_count()} logical CPUs, jobs {jobs}); seconds, wall clock")
    print("RESULT | module | cold (fresh target) | no-op | one-line edit, min / max of %d |" % (2 * reps))
    print("RESULT |---|---|---|---|")
    tmp = tempfile.mkdtemp(prefix="pe-guest-times-")
    try:
        for name, ws, args, path, a, b in MODULES:
            env = env_for(os.path.join(tmp, ws.replace("/", "_").replace(".", "root")))
            cold = build(ws, args, env)
            noop = build(ws, args, env)
            ts = []
            if path:
                try:
                    for _ in range(reps):
                        edit(path, a, b)
                        ts.append(build(ws, args, env))
                        edit(path, b, a)
                        ts.append(build(ws, args, env))
                finally:
                    s = open(os.path.join(ROOT, path), encoding="utf-8").read()
                    if b in s:
                        edit(path, b, a)
            e = f"{min(ts):.2f} / {max(ts):.2f}" if ts else "n/a"
            print(f"RESULT | {name} | {cold:.2f} | {noop:.2f} | {e} |", flush=True)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
