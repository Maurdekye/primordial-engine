"""iter-bench: the pinned toolchain a script build needs, its size, and whether running it by absolute path (no
rustup proxy) gives the same module. For the build-asymmetry draft (user ruling): choice 4 (every
install carries the toolchain: how big?) and prevention 2 (@fable-reviewer: run the pinned cargo and rustc by
absolute path, since rustup's proxy picks the compiler from the working folder's rust-toolchain file).

1. Copies the minimal set out of the rustup toolchain into --out (bin/cargo, bin/rustc, the rustc_driver library,
   lib/rustlib/<host>/bin/rust-lld and gcc-ld, the whole wasm32-unknown-unknown std) and reports its size, the full
   toolchain's, and the minimal set's size as an xz tar (what a download would carry).
2. Builds the lamp (with pe-sdk) and the 6.4k-line guest, clean, with the build service's flags, four ways:
     proxy-repo     `cargo` through rustup, working folder = the repo (its rust-toolchain.toml pins 1.99.0)
     proxy-elsewhere `cargo` through rustup, working folder outside any rust-toolchain file
     abs-rustup     the rustup toolchain's own cargo/rustc by absolute path, rustup's folders off PATH
     abs-min        the minimal copy by absolute path, rustup's folders off PATH
   and prints each module's sha256 (or the failure).

usage (repo root of a ws/logic checkout; under heavy.sh):
  python -I scripts/iter_bench_toolchain_min.py --toolchain <rustup toolchain dir> --out <empty dir outside the repo>
     --guests-root <dir holding spikes/iter-bench/guests>
Raw: spikes/out/iter-bench/toolchain/results.json.
"""

import hashlib
import io
import json
import lzma
import os
import shutil
import subprocess
import sys
import tarfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from iter_bench_build_budget import TEMPLATE, run_in_job  # noqa: E402
from iter_bench_logic_edit import flags_from_build_rs  # noqa: E402
from cargo_output import target_dir  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WASM = "wasm32-unknown-unknown"
HOST = "x86_64-pc-windows-msvc"


def tree_size(p):
    t = n = 0
    for d, _, fs in os.walk(p):
        for f in fs:
            t += os.path.getsize(os.path.join(d, f))
            n += 1
    return t, n


def minimal(tc, out):
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(os.path.join(out, "bin"))
    for f in os.listdir(os.path.join(tc, "bin")):
        if f in ("cargo.exe", "rustc.exe") or (f.startswith("rustc_driver-") and f.endswith(".dll")):
            shutil.copy2(os.path.join(tc, "bin", f), os.path.join(out, "bin", f))
    hb = os.path.join("lib", "rustlib", HOST, "bin")
    os.makedirs(os.path.join(out, hb, "gcc-ld"))
    shutil.copy2(os.path.join(tc, hb, "rust-lld.exe"), os.path.join(out, hb, "rust-lld.exe"))
    for f in os.listdir(os.path.join(tc, hb, "gcc-ld")):
        if f.endswith(".exe"):
            shutil.copy2(os.path.join(tc, hb, "gcc-ld", f), os.path.join(out, hb, "gcc-ld", f))
    shutil.copytree(os.path.join(tc, "lib", "rustlib", WASM), os.path.join(out, "lib", "rustlib", WASM))


def xz_size(p):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:xz", preset=6) as t:
        t.add(p, arcname="toolchain")
    return buf.tell()


def main():
    a = sys.argv[1:]
    tc = os.path.abspath(a[a.index("--toolchain") + 1])
    out = os.path.abspath(a[a.index("--out") + 1])
    groot = os.path.abspath(a[a.index("--guests-root") + 1])
    res_dir = os.path.join(ROOT, "spikes", "out", "iter-bench", "toolchain")
    os.makedirs(res_dir, exist_ok=True)
    res = {"start": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "toolchain": os.path.basename(tc)}

    full, nfull = tree_size(tc)
    minimal(tc, out)
    mn, nmin = tree_size(out)
    t0 = time.perf_counter()
    xz = xz_size(out)
    res["size"] = {"full_mib": round(full / 2**20, 1), "full_files": nfull, "minimal_mib": round(mn / 2**20, 1),
                   "minimal_files": nmin, "minimal_xz_mib": round(xz / 2**20, 1),
                   "xz_seconds": round(time.perf_counter() - t0, 1),
                   "minimal_list": sorted(os.path.relpath(os.path.join(d, f), out).replace("\\", "/")
                                          for d, _, fs in os.walk(out) for f in fs
                                          if not f.endswith(".rlib") and not f.endswith(".rmeta"))}
    print(json.dumps(res["size"], indent=1), flush=True)

    work = os.path.join(res_dir, "work")
    shutil.rmtree(work, ignore_errors=True)
    sdk = os.path.join(ROOT, "crates", "pe-sdk").replace("\\", "/")
    guests = {
        "lamp": (os.path.join(ROOT, "crates", "pe-logic", "tests", "scripts", "lamp", "src"),
                 '\n[dependencies]\npe-sdk = { path = "%s" }\n' % sdk, True),
        "guest6k": (os.path.join(groot, "spikes", "iter-bench", "guests", "logic_mono", "src"), "", False),
    }
    profile, codegen = flags_from_build_rs(ROOT)
    no_rustup_path = os.pathsep.join(p for p in os.environ.get("PATH", "").split(os.pathsep)
                                     if ".cargo" not in p and ".rustup" not in p)
    elsewhere = os.path.join(os.environ.get("TEMP", os.path.dirname(out)), "iter-bench-no-toolchain-file")
    os.makedirs(elsewhere, exist_ok=True)
    ways = {
        "proxy-repo": ("cargo", None, ROOT, None),
        "proxy-elsewhere": ("cargo", None, elsewhere, None),
        "abs-rustup": (os.path.join(tc, "bin", "cargo.exe"), os.path.join(tc, "bin", "rustc.exe"), elsewhere,
                       no_rustup_path),
        "abs-min": (os.path.join(out, "bin", "cargo.exe"), os.path.join(out, "bin", "rustc.exe"), elsewhere,
                    no_rustup_path),
    }
    res["builds"] = {}
    for name, (src, deps, forbid) in guests.items():
        crate = os.path.join(work, name)
        shutil.copytree(src, os.path.join(crate, "src"))
        open(os.path.join(crate, "Cargo.toml"), "w", encoding="utf-8", newline="\n").write(TEMPLATE.format(deps=deps))
        for way, (cargo, rustc, cwd, path) in ways.items():
            tgt = target_dir(ROOT, "toolchain/%s-%s" % (name, way))
            shutil.rmtree(tgt, ignore_errors=True)
            env = dict(os.environ)
            for k in ("RUSTFLAGS", "RUSTUP_TOOLCHAIN", "RUSTC", "CARGO"):
                env.pop(k, None)
            env.update({"CARGO_ENCODED_RUSTFLAGS": "\x1f".join(codegen), "CARGO_INCREMENTAL": "0",
                        "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "", "CARGO_TARGET_DIR": tgt})
            if rustc:
                env["RUSTC"] = rustc
            if path is not None:
                env["PATH"] = path
            cmd = [cargo, "rustc", "--release", "--lib", "--target", WASM, "--manifest-path",
                   os.path.join(crate, "Cargo.toml"), "--offline", "--message-format=short"]
            for p in profile:
                cmd += ["--config", p]
            if forbid:
                cmd += ["--", "-F", "unsafe_code"]
            ver = subprocess.run([rustc or "rustc", "-V"], capture_output=True, text=True, cwd=cwd,
                                 env=env).stdout.strip()
            rc, dt, peak, _, log = run_in_job(cmd, env, cwd=cwd)
            wasm = os.path.join(tgt, WASM, "release", "script.wasm")
            row = {"exit": rc, "rustc": ver, "wall_s": round(dt, 2)}
            if os.path.exists(wasm):
                row["sha256"] = hashlib.sha256(open(wasm, "rb").read()).hexdigest()[:16]
            if rc != 0:
                row["log_tail"] = log[-600:]
            print(name, way, json.dumps(row), flush=True)
            res["builds"]["%s %s" % (name, way)] = row
            shutil.rmtree(tgt, ignore_errors=True)
    shutil.rmtree(work, ignore_errors=True)
    res["end"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    json.dump(res, open(os.path.join(res_dir, "results.json"), "w", encoding="utf-8", newline="\n"), indent=1)


if __name__ == "__main__":
    main()
