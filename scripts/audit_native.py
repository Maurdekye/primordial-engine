"""Audit a Cargo dependency graph for non-Rust code (the "100% Rust" rule).

For every package reachable from the root (normal + build edges, dev edges excluded) on the given
target triple, report:
  * whether it has a build script,
  * which native-build helpers it depends on at build time (cc, cmake, nasm-rs, cxx-build,
    bindgen, pkg-config, vcpkg, system-deps),
  * how many C / C++ / assembly source files ship in the package,
  * its `links` key (declares a native library it links),
  * whether it is a Windows import-library crate (prebuilt .lib stubs for OS DLLs).

usage: python -I scripts/audit_native.py <crate-dir>... [--target TRIPLE]... [--md OUT.md]
"""

import json
import os
import subprocess
import sys

NATIVE_BUILD = {"cc", "cmake", "nasm-rs", "cxx-build", "bindgen", "autotools"}
SYSLIB_BUILD = {"pkg-config", "vcpkg", "system-deps"}
SRC_EXT = {".c": "c", ".cc": "c++", ".cpp": "c++", ".cxx": "c++", ".S": "asm", ".s": "asm", ".asm": "asm"}


def count_native_sources(pkg_dir):
    counts = {}
    for dirpath, dirnames, filenames in os.walk(pkg_dir):
        dirnames[:] = [d for d in dirnames if d not in ("target", ".git")]
        for fn in filenames:
            ext = os.path.splitext(fn)[1]
            if ext in SRC_EXT:
                counts[SRC_EXT[ext]] = counts.get(SRC_EXT[ext], 0) + 1
    return counts


def built_set(crate_dir, triple):
    """Packages cargo would actually build (real feature resolver), via `cargo tree`."""
    out = subprocess.run(
        ["cargo", "tree", "-e", "normal,build", "--target", triple, "--prefix", "none",
         "--format", "{p}", "--manifest-path", os.path.join(crate_dir, "Cargo.toml"), "--workspace"],
        capture_output=True, text=True, encoding="utf-8", errors="replace")
    if out.returncode != 0:
        raise SystemExit(f"cargo tree failed for {crate_dir}: " + out.stderr[-2000:])
    built = set()
    for line in out.stdout.splitlines():
        parts = line.strip().split(" ")
        if len(parts) >= 2 and parts[1].startswith("v"):
            built.add((parts[0], parts[1][1:]))
    return built


def audit(crate_dir, triple):
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--filter-platform", triple,
         "--manifest-path", os.path.join(crate_dir, "Cargo.toml")],
        capture_output=True, text=True, encoding="utf-8", errors="replace")
    if out.returncode != 0:
        raise SystemExit(f"cargo metadata failed for {crate_dir}:\n{out.stderr[-2000:]}")
    meta = json.loads(out.stdout)
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]
    roots = [root] if root else list(meta["workspace_members"])
    seen, stack = set(), list(roots)
    while stack:
        nid = stack.pop()
        if nid in seen:
            continue
        seen.add(nid)
        for d in nodes[nid]["deps"]:
            kinds = {k["kind"] for k in d["dep_kinds"]}
            if kinds & {None, "build"}:
                stack.append(d["pkg"])
    built = built_set(crate_dir, triple)
    rows = []
    for nid in sorted(seen, key=lambda i: pkgs[i]["name"]):
        if nid in roots:
            continue
        p = pkgs[nid]
        if (p["name"], p["version"]) not in built:
            continue  # only in cargo-metadata's broader feature unification; not actually built
        build_script = any("custom-build" in t["kind"] for t in p["targets"])
        bdeps = {pkgs[d["pkg"]]["name"] for d in nodes[nid]["deps"]
                 if any(k["kind"] == "build" for k in d["dep_kinds"])}
        native = sorted(bdeps & NATIVE_BUILD)
        syslib = sorted(bdeps & SYSLIB_BUILD)
        srcs = count_native_sources(os.path.dirname(p["manifest_path"]))
        importlib = p["name"].startswith("windows_") and any(
            a in p["name"] for a in ("x86_64", "i686", "aarch64"))
        if native and srcs:
            cls = "COMPILES NATIVE CODE"
        elif native:
            cls = "native build helper (no C/asm files shipped; inspect)"
        elif importlib:
            cls = "prebuilt Windows import library (OS DLL stubs)"
        elif p.get("links") or syslib:
            cls = "links a system library"
        elif srcs:
            cls = "ships C/asm files but no native build helper (inspect)"
        elif build_script:
            cls = "rust build.rs only"
        else:
            cls = "pure Rust"
        rows.append({"name": p["name"], "version": p["version"], "class": cls, "native": native,
                     "syslib": syslib, "links": p.get("links"), "srcs": srcs})
    return rows


def main():
    args = sys.argv[1:]
    triples, dirs, md = [], [], None
    i = 0
    while i < len(args):
        if args[i] == "--target":
            triples.append(args[i + 1]); i += 2
        elif args[i] == "--md":
            md = args[i + 1]; i += 2
        else:
            dirs.append(args[i]); i += 1
    triples = triples or ["x86_64-pc-windows-msvc"]
    lines = ["# Native-code audit", "",
             "Generated by `scripts/audit_native.py`: `cargo metadata` for package facts, restricted to the packages `cargo tree -e normal,build` says are actually built.", "","Classes: COMPILES NATIVE CODE = a native build helper (cc/cmake/...) is a build dependency AND the package ships C/C++/asm sources; it may still be feature-gated off inside build.rs, so each hit is checked by hand in the spike report.", ""]
    for d in dirs:
        name = os.path.basename(os.path.normpath(d))
        lines.append(f"## {name}")
        lines.append("")
        for t in triples:
            rows = audit(d, t)
            flagged = [r for r in rows if r["class"] not in ("pure Rust", "rust build.rs only")]
            pure = sum(1 for r in rows if r["class"] in ("pure Rust", "rust build.rs only"))
            verdict = "100% Rust" if not any(r["class"] == "COMPILES NATIVE CODE" for r in rows) else "NOT 100% Rust"
            summary = f"{name} [{t}]: {len(rows)} packages, {pure} pure-Rust, {len(flagged)} flagged -> {verdict}"
            print(summary)
            lines.append(f"**{t}**: {len(rows)} packages, {pure} pure Rust, {len(flagged)} flagged: **{verdict}**")
            lines.append("")
            if flagged:
                lines.append("| package | class | native build deps | sys-lib deps | links | C/C++/asm files |")
                lines.append("|---|---|---|---|---|---|")
                for r in flagged:
                    print(f"    {r['name']} {r['version']}: {r['class']} {r['native'] or ''} {r['syslib'] or ''} {r['links'] or ''} {r['srcs'] or ''}")
                    lines.append(f"| {r['name']} {r['version']} | {r['class']} | {', '.join(r['native'])} | "
                                 f"{', '.join(r['syslib'])} | {r['links'] or ''} | "
                                 f"{', '.join(f'{k}: {v}' for k, v in r['srcs'].items())} |")
                lines.append("")
    if md:
        with open(md, "w", newline="\n", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
