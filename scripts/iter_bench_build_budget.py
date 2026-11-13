"""iter-bench: what a clean script build costs in memory and time, for logic.md 6.2a's uniform build budget.

Each build runs in its own Windows job object (cargo and every process it starts: rustc, rust-lld), started
suspended so nothing escapes before it is assigned. The job reports its peak committed memory (PeakJobMemoryUsed,
all processes at once) and the largest single process's peak (PeakProcessMemoryUsed): the same measure a job memory
limit would enforce. Wall time is from process start to exit.

The build is the service's: the profile and codegen lists of crates/pe-logic/src/build.rs, incremental off, a fresh
target dir per build (clean), the crate's Cargo.toml written from 4.1a's template (package `script`, edition 2024,
pe-sdk the only dependency). Guests:
  lamp       crates/pe-logic/tests/scripts/lamp (with pe-sdk, -F unsafe_code as the service passes)
  guest6k    spikes/iter-bench/guests/logic_mono, the 6.4k-line benchmark guest (no pe-sdk; it declares its own
             imports with `unsafe extern`, so -F unsafe_code is left out for it)

usage (repo root of a ws/logic checkout; under heavy.sh):
  python -I scripts/iter_bench_build_budget.py --guests-root <dir holding spikes/iter-bench/guests> [--reps N] [--tag T]
Raw: spikes/out/iter-bench/build-budget[-T]/builds.jsonl, summary.json.
"""

import ctypes
import ctypes.wintypes as W
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from iter_bench_logic_edit import flags_from_build_rs  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cargo_output import target_dir

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WASM = "wasm32-unknown-unknown"
k32 = ctypes.WinDLL("kernel32", use_last_error=True)
ntdll = ctypes.WinDLL("ntdll")


class IO_COUNTERS(ctypes.Structure):
    _fields_ = [(n, ctypes.c_ulonglong) for n in ("r_ops", "w_ops", "o_ops", "r_bytes", "w_bytes", "o_bytes")]


class BASIC(ctypes.Structure):
    _fields_ = [("PerProcessUserTimeLimit", ctypes.c_longlong), ("PerJobUserTimeLimit", ctypes.c_longlong),
                ("LimitFlags", W.DWORD), ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", W.DWORD),
                ("Affinity", ctypes.c_size_t), ("PriorityClass", W.DWORD), ("SchedulingClass", W.DWORD)]


class EXTENDED(ctypes.Structure):
    _fields_ = [("Basic", BASIC), ("Io", IO_COUNTERS), ("ProcessMemoryLimit", ctypes.c_size_t),
                ("JobMemoryLimit", ctypes.c_size_t), ("PeakProcessMemoryUsed", ctypes.c_size_t),
                ("PeakJobMemoryUsed", ctypes.c_size_t)]


k32.CreateJobObjectW.restype = W.HANDLE
k32.CreateJobObjectW.argtypes = [ctypes.c_void_p, W.LPCWSTR]
k32.AssignProcessToJobObject.argtypes = [W.HANDLE, W.HANDLE]
k32.QueryInformationJobObject.argtypes = [W.HANDLE, ctypes.c_int, ctypes.c_void_p, W.DWORD, ctypes.c_void_p]
ntdll.NtResumeProcess.argtypes = [W.HANDLE]


def run_in_job(cmd, env, cwd=None):
    job = k32.CreateJobObjectW(None, None)
    assert job, ctypes.get_last_error()
    t0 = time.perf_counter()
    p = subprocess.Popen(cmd, env=env, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         creationflags=0x00000004)  # CREATE_SUSPENDED
    assert k32.AssignProcessToJobObject(job, int(p._handle)), ctypes.get_last_error()
    ntdll.NtResumeProcess(int(p._handle))
    out, _ = p.communicate()
    dt = time.perf_counter() - t0
    info = EXTENDED()
    assert k32.QueryInformationJobObject(job, 9, ctypes.byref(info), ctypes.sizeof(info), None)
    k32.CloseHandle(job)
    return p.returncode, dt, info.PeakJobMemoryUsed, info.PeakProcessMemoryUsed, out.decode("utf-8", "replace")


TEMPLATE = """[package]
name = "script"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[lib]
crate-type = ["cdylib"]
path = "src/lib.rs"
{deps}"""


def stats(xs):
    return {"median": round(statistics.median(xs), 3), "min": round(min(xs), 3), "max": round(max(xs), 3),
            "n": len(xs)}


def main():
    a = sys.argv[1:]
    groot = os.path.abspath(a[a.index("--guests-root") + 1])
    reps = int(a[a.index("--reps") + 1]) if "--reps" in a else 3
    tag = a[a.index("--tag") + 1] if "--tag" in a else ""
    out = os.path.join(ROOT, "spikes", "out", "iter-bench", "build-budget" + ("-" + tag if tag else ""))
    os.makedirs(out, exist_ok=True)
    work = os.path.join(out, "work")
    shutil.rmtree(work, ignore_errors=True)
    sdk = os.path.join(ROOT, "crates", "pe-sdk").replace("\\", "/")
    guests = {
        "lamp": (os.path.join(ROOT, "crates", "pe-logic", "tests", "scripts", "lamp", "src"),
                 '\n[dependencies]\npe-sdk = { path = "%s" }\n' % sdk, True),
        "guest6k": (os.path.join(groot, "spikes", "iter-bench", "guests", "logic_mono", "src"), "", False),
    }
    profile, codegen = flags_from_build_rs(ROOT)
    meta = {"rustc": subprocess.run(["rustc", "-V"], capture_output=True, text=True).stdout.strip(),
            "reps": reps, "profile": profile, "codegen": codegen, "heavy_prio": os.environ.get("HEAVY_PRIO", "idle"),
            "jobs": os.environ.get("CARGO_BUILD_JOBS"), "start": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    rows = []
    f = open(os.path.join(out, "builds.jsonl"), "w", encoding="utf-8", newline="\n")
    f.write(json.dumps({"meta": meta}) + "\n")
    summary = {"meta": meta, "guests": {}}
    for name, (src, deps, forbid) in guests.items():
        crate = os.path.join(work, name)
        shutil.copytree(src, os.path.join(crate, "src"))
        src_bytes = sum(os.path.getsize(os.path.join(d, x)) for d, _, fs in os.walk(os.path.join(crate, "src"))
                        for x in fs)
        open(os.path.join(crate, "Cargo.toml"), "w", encoding="utf-8", newline="\n").write(
            TEMPLATE.format(deps=deps))
        mine = []
        for r in range(reps):
            tgt = target_dir(ROOT, "build-budget/" + os.path.basename(out) + "/%s-%d" % (name, r))
            env = dict(os.environ)
            env.pop("RUSTFLAGS", None)
            env.update({"CARGO_ENCODED_RUSTFLAGS": "\x1f".join(codegen), "CARGO_INCREMENTAL": "0",
                        "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "", "CARGO_TARGET_DIR": tgt})
            cmd = ["cargo", "rustc", "--release", "--lib", "--target", WASM, "--manifest-path",
                   os.path.join(crate, "Cargo.toml"), "--offline", "--message-format=short"]
            for p in profile:
                cmd += ["--config", p]
            if forbid:
                cmd += ["--", "-F", "unsafe_code"]
            rc, dt, peak_job, peak_proc, log = run_in_job(cmd, env)
            wasm = os.path.join(tgt, WASM, "release", "script.wasm")
            row = {"guest": name, "rep": r, "exit": rc, "wall_s": round(dt, 3), "peak_job_mib": round(peak_job / 2**20, 1),
                   "peak_process_mib": round(peak_proc / 2**20, 1), "src_bytes": src_bytes,
                   "wasm_bytes": os.path.getsize(wasm) if os.path.exists(wasm) else None}
            if rc != 0:
                row["log_tail"] = log[-1500:]
            print(json.dumps(row), flush=True)
            f.write(json.dumps(row) + "\n")
            mine.append(row)
            shutil.rmtree(tgt, ignore_errors=True)
        ok = [x for x in mine if x["exit"] == 0]
        if ok:
            summary["guests"][name] = {k: stats([x[k] for x in ok]) for k in ("wall_s", "peak_job_mib",
                                                                              "peak_process_mib")}
            summary["guests"][name]["src_bytes"] = src_bytes
    meta["end"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    json.dump(summary, open(os.path.join(out, "summary.json"), "w", encoding="utf-8"), indent=1)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
