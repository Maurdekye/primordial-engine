"""iter-bench: can two machines' builds of one script version differ? Probes for the build-asymmetry ruling
(user: a script one member can build and another can't is a divergence; prevent it).

Two experiments, both with the build service's own command (the profile and codegen lists of
crates/pe-logic/src/build.rs, incremental off, a fresh target dir per build), each build in a Windows job object
(peak committed memory of cargo + rustc + rust-lld together, as iter_bench_build_budget.py):

  jobs   The same guest at CARGO_BUILD_JOBS = 1, 4 and 16: does the module's sha256 move with the core count, and
         does the peak memory (what 6.2a item 5's 1 GiB cap would measure)?
  leak   Inputs the build service does not pin today: build.rs neither clears the child's environment nor sets
         its working directory (crates/pe-logic/src/build.rs:267-291). A guest that reads `option_env!` and
         `include_bytes!` of an absolute path is built (a) as is, (b) with an environment variable set, (c) from a
         working directory whose `.cargo/config.toml` sets that variable in `[env]`, (d) with the absolute file's
         content changed. Different hashes mean the module depends on the building machine, not only on V.

usage (repo root of a ws/logic checkout; under heavy.sh):
  python -I scripts/iter_bench_build_asym.py --guests-root <dir holding spikes/iter-bench/guests> [--reps N]
Raw: spikes/out/iter-bench/build-asym/builds.jsonl, summary.json.
"""

import hashlib
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from iter_bench_build_budget import TEMPLATE, run_in_job  # noqa: E402
from iter_bench_logic_edit import flags_from_build_rs  # noqa: E402
from cargo_output import target_dir  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WASM = "wasm32-unknown-unknown"
VAR = "PE_ASYM_PROBE"

LEAK_SRC = r'''#![no_std]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
static ENV: &str = match option_env!("PE_ASYM_PROBE") {
    Some(s) => s,
    None => "",
};
static FILE: &[u8] = include_bytes!("@ABS@");
#[unsafe(no_mangle)]
pub extern "C" fn probe(i: usize) -> u32 {
    ENV.as_bytes().get(i).copied().unwrap_or(0) as u32 + FILE.get(i).copied().unwrap_or(0) as u32
}
'''


def stats(xs):
    return {"median": round(statistics.median(xs), 1), "min": round(min(xs), 1), "max": round(max(xs), 1),
            "n": len(xs)}


def build(crate, profile, codegen, forbid, leaf, jobs=None, extra_env=None, cwd=None):
    tgt = target_dir(ROOT, "build-asym/" + leaf)
    shutil.rmtree(tgt, ignore_errors=True)
    env = dict(os.environ)
    env.pop("RUSTFLAGS", None)
    env.pop(VAR, None)
    env.update({"CARGO_ENCODED_RUSTFLAGS": "\x1f".join(codegen), "CARGO_INCREMENTAL": "0",
                "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "", "CARGO_TARGET_DIR": tgt})
    if jobs is not None:
        env["CARGO_BUILD_JOBS"] = str(jobs)
    env.update(extra_env or {})
    cmd = ["cargo", "rustc", "--release", "--lib", "--target", WASM, "--manifest-path",
           os.path.join(crate, "Cargo.toml"), "--offline", "--message-format=short"]
    for p in profile:
        cmd += ["--config", p]
    if forbid:
        cmd += ["--", "-F", "unsafe_code"]
    rc, dt, peak_job, peak_proc, log = run_in_job(cmd, env, cwd=cwd)
    wasm = os.path.join(tgt, WASM, "release", "script.wasm")
    row = {"exit": rc, "wall_s": round(dt, 3), "peak_job_mib": round(peak_job / 2**20, 1),
           "peak_process_mib": round(peak_proc / 2**20, 1), "jobs": env.get("CARGO_BUILD_JOBS")}
    if os.path.exists(wasm):
        b = open(wasm, "rb").read()
        row.update({"wasm_bytes": len(b), "sha256": hashlib.sha256(b).hexdigest()[:16]})
    if rc != 0:
        row["log_tail"] = log[-1500:]
    shutil.rmtree(tgt, ignore_errors=True)
    return row


def main():
    a = sys.argv[1:]
    groot = os.path.abspath(a[a.index("--guests-root") + 1])
    reps = int(a[a.index("--reps") + 1]) if "--reps" in a else 3
    out = os.path.join(ROOT, "spikes", "out", "iter-bench", "build-asym")
    os.makedirs(out, exist_ok=True)
    work = os.path.join(out, "work")
    shutil.rmtree(work, ignore_errors=True)
    sdk = os.path.join(ROOT, "crates", "pe-sdk").replace("\\", "/")
    profile, codegen = flags_from_build_rs(ROOT)
    meta = {"rustc": subprocess.run(["rustc", "-vV"], capture_output=True, text=True).stdout.strip().splitlines(),
            "reps": reps, "profile": profile, "codegen": codegen, "heavy_prio": os.environ.get("HEAVY_PRIO", "idle"),
            "cpus": os.cpu_count(), "start": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    f = open(os.path.join(out, "builds.jsonl"), "w", encoding="utf-8", newline="\n")
    f.write(json.dumps({"meta": meta}) + "\n")
    summary = {"meta": meta, "jobs": {}, "leak": {}}

    def emit(row):
        print(json.dumps(row), flush=True)
        f.write(json.dumps(row) + "\n")

    # Experiment 1: the core count.
    guests = {
        "lamp": (os.path.join(ROOT, "crates", "pe-logic", "tests", "scripts", "lamp", "src"),
                 '\n[dependencies]\npe-sdk = { path = "%s" }\n' % sdk, True),
        "guest6k": (os.path.join(groot, "spikes", "iter-bench", "guests", "logic_mono", "src"), "", False),
    }
    for name, (src, deps, forbid) in guests.items():
        crate = os.path.join(work, name)
        shutil.copytree(src, os.path.join(crate, "src"))
        open(os.path.join(crate, "Cargo.toml"), "w", encoding="utf-8", newline="\n").write(TEMPLATE.format(deps=deps))
        for jobs in (1, 4, 16):
            rows = []
            for r in range(reps):
                row = build(crate, profile, codegen, forbid, "%s-j%d-%d" % (name, jobs, r), jobs=jobs)
                row.update({"exp": "jobs", "guest": name, "rep": r})
                emit(row)
                rows.append(row)
            ok = [x for x in rows if x["exit"] == 0]
            summary["jobs"]["%s j%d" % (name, jobs)] = {
                "peak_job_mib": stats([x["peak_job_mib"] for x in ok]) if ok else None,
                "wall_s": stats([x["wall_s"] for x in ok]) if ok else None,
                "sha256": sorted({x.get("sha256") for x in ok}), "failed": len(rows) - len(ok)}

    # Experiment 2: inputs from the building machine.
    crate = os.path.join(work, "leak")
    os.makedirs(os.path.join(crate, "src"))
    absfile = os.path.join(out, "work", "outside.bin")
    open(absfile, "wb").write(b"A")
    open(os.path.join(crate, "src", "lib.rs"), "w", encoding="utf-8", newline="\n").write(
        LEAK_SRC.replace("@ABS@", absfile.replace("\\", "/")))
    open(os.path.join(crate, "Cargo.toml"), "w", encoding="utf-8", newline="\n").write(TEMPLATE.format(deps=""))
    cfgdir = os.path.join(work, "cwd-with-config")
    os.makedirs(os.path.join(cfgdir, ".cargo"))
    open(os.path.join(cfgdir, ".cargo", "config.toml"), "w", encoding="utf-8", newline="\n").write(
        '[env]\n%s = "from-config"\n' % VAR)
    cases = [("a plain", {}, None), ("b env var set", {VAR: "from-env"}, None),
             ("c cwd has .cargo/config.toml [env]", {}, cfgdir), ("a plain again", {}, None)]
    for label, extra, cwd in cases:
        row = build(crate, profile, codegen, False, "leak-" + label.split()[0], extra_env=extra, cwd=cwd)
        row.update({"exp": "leak", "case": label})
        emit(row)
        summary["leak"][label] = row.get("sha256") or ("exit %d" % row["exit"])
    open(absfile, "wb").write(b"B")
    row = build(crate, profile, codegen, False, "leak-d")
    row.update({"exp": "leak", "case": "d absolute include_bytes file changed"})
    emit(row)
    summary["leak"]["d absolute include_bytes file changed"] = row.get("sha256") or ("exit %d" % row["exit"])

    meta["end"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    json.dump(summary, open(os.path.join(out, "summary.json"), "w", encoding="utf-8"), indent=1)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
