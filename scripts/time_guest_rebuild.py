"""Spike d5: time rebuilding a WASM game-logic guest after a one-line edit.

Edits a constant in spikes/wasm-guests/logic-v2/src/lib.rs, rebuilds, reverts, rebuilds (3x per
profile), restores the original source, and prints wall-clock timings.

usage (from the repo root): python -I scripts/time_guest_rebuild.py
"""

import os
import subprocess
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GUESTS = os.path.join(ROOT, "spikes", "wasm-guests")
SRC = os.path.join(GUESTS, "logic-v2", "src", "lib.rs")
A, B = "s.spin * 1.07 + 0.01", "s.spin * 1.08 + 0.01"


def build(args):
    t = time.perf_counter()
    r = subprocess.run(["cargo", "build", "-q", *args, "--target", "wasm32-unknown-unknown", "-p", "logic_v2"],
                       cwd=GUESTS, capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    return time.perf_counter() - t


def main():
    original = open(SRC, encoding="utf-8").read()
    assert A in original
    try:
        for name, args in [("release (opt-level=s, lto, cgu=1)", ["--release"]), ("dev (debug)", [])]:
            build(args)  # warm
            ts = []
            for _ in range(3):
                open(SRC, "w", encoding="utf-8", newline="\n").write(original.replace(A, B))
                ts.append(build(args))
                open(SRC, "w", encoding="utf-8", newline="\n").write(original)
                ts.append(build(args))
            print(f"{name}: " + ", ".join(f"{x:.2f}s" for x in ts) + f"  (min {min(ts):.2f}s, max {max(ts):.2f}s)")
    finally:
        open(SRC, "w", encoding="utf-8", newline="\n").write(original)


if __name__ == "__main__":
    main()
