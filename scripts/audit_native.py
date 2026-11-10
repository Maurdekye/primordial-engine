"""Audit Cargo dependency graphs for non-Rust code (the "100% Rust" ruling, D1), and gate landings on it.

What it reports, per package that cargo actually builds (normal and build edges; dev edges excluded) for a
target triple, from `cargo tree` (the real feature resolver) and `cargo metadata` (package facts):
  * whether it has a build script, and which native-build helpers its build dependencies pull in
    (cc, cmake, nasm-rs, cxx-build, bindgen, autotools) or system-library finders (pkg-config, vcpkg,
    system-deps), from the edges `cargo tree` resolves, not from `cargo metadata`, whose broader
    feature unification over-reports (an early run flagged ring under rustls-graviola, and the
    2026-11-10 run flagged khronos-egl's pkg-config, which no build enables);
  * how many C / C++ / assembly / Objective-C sources and prebuilt native libraries ship in the package,
    its `links` key, and whether its build script asks to link a native library;
  * for the workspace's own crates, also `#[link(...)]` attributes;
  * how many of its Rust files use inline assembly (`asm!`, `global_asm!`, `naked_asm!`): Rust by the
    ruling's wording (D1, design 20.2), listed as its own class. A text scan, so a hit may sit behind a
    `cfg` for another target.

Each package gets the first class that fits, in this order (the names the allow file uses):
  compiles-native     ships native sources, has a build script and a native-build helper
  native-helper       has a build script and a native-build helper, but ships no native sources
  native-sources      ships native sources and has a build script, but no helper crate (it could still run
                      a compiler itself)
  windows-import-lib  windows_<arch>_<abi>: prebuilt import stubs for Windows system DLLs
  prebuilt-native     ships prebuilt native libraries or objects and has a build script
  links-native        a `links` key, a system-library finder, a build script that emits rustc-link-lib,
                      or (own crates only) a #[link] attribute
  inline-asm          Rust files with inline assembly
  inert-sources       ships native sources but has no build script, so cargo never compiles them
  pure                none of the above
A package that cargo resolves with different features in different places is classified once, from the
union of what it is built with.

Two modes.

  Report (as before): python -I scripts/audit_native.py <workspace-dir>... [--target TRIPLE]...
                      [--md OUT.md] [--allow FILE] [--offline]
    audits the named workspaces; with --allow it also marks what the allow file accepts. Exits 0.

  Check (the landing gate, scripts/gate.sh): python -I scripts/audit_native.py --check [--tree DIR]
                      [--allow FILE] [--md OUT.md] [--offline]
    finds every Cargo workspace in DIR (default: the current directory; its tracked and untracked,
    not ignored, files if it is a git work tree, else every file under it), audits each one, the
    workspace as a whole and each of its crates on its own, for the allow file's targets, and exits
    0 if every flagged package is accepted, 1 if not, 2 on an error. New crates are covered as they
    appear: a crate that is in no workspace the gate audits, and not under an `exclude` line, fails.
    --allow defaults to DIR/scripts/native_allow.txt; its format is described in that file.
"""

import fnmatch
import json
import os
import re
import subprocess
import sys

NATIVE_BUILD = {"cc", "cmake", "nasm-rs", "cxx-build", "bindgen", "autotools"}
SYSLIB_BUILD = {"pkg-config", "vcpkg", "system-deps"}
SRC_EXT = {".c": "c", ".cc": "c++", ".cpp": "c++", ".cxx": "c++", ".S": "asm", ".s": "asm", ".asm": "asm",
           ".m": "objc", ".mm": "objc++"}
BIN_EXT = {".a", ".lib", ".o", ".obj", ".so", ".dylib", ".dll"}
ASM_MACROS = (b"asm!(", b"asm! (", b"naked_asm!(")
# #[link(...)] links a native library; #[link(wasm_import_module = "...")] only names a wasm import module.
LINK_ATTR = re.compile(rb"#\[link\((?!\s*wasm_import_module\s*=)")
SKIP_DIRS = {"target", ".git", ".tools", ".worktrees"}
CLASSES = ["compiles-native", "native-helper", "native-sources", "windows-import-lib", "prebuilt-native",
           "links-native", "inline-asm", "inert-sources", "pure"]
IMPORTLIB = re.compile(r"^windows_(x86_64|i686|aarch64)_(msvc|gnu|gnullvm)$")


class GateError(Exception):
    pass


def run(cmd, cwd=None):
    out = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if out.returncode != 0:
        raise GateError(f"{' '.join(cmd)} failed (exit {out.returncode}) in {cwd or os.getcwd()}:\n"
                        + out.stderr.strip()[-3000:])
    return out.stdout


# ---------------------------------------------------------------------------------------------
# The allow file

class Allow:
    def __init__(self):
        self.targets, self.target_overrides, self.excludes, self.classes, self.entries = [], [], [], {}, []

    @staticmethod
    def parse(path):
        a = Allow()
        with open(path, encoding="utf-8") as f:
            for n, raw in enumerate(f, 1):
                line = raw.strip()
                if not line or line.startswith("#"):
                    continue
                body, sep, reason = line.partition(" -- ")
                words = body.split()
                kind = words[0]
                where = f"{path}:{n}"
                if kind == "target" and len(words) == 2 and not sep:
                    a.targets.append(words[1])
                elif kind == "targets" and len(words) >= 3:
                    a.target_overrides.append((words[1].strip("/"), words[2:], reason.strip(), where))
                elif kind == "exclude" and len(words) == 2 and words[1].strip("/") not in ("", "."):
                    a.excludes.append((words[1].strip("/"), reason.strip(), where))
                elif kind == "class" and len(words) == 2 and words[1] in CLASSES:
                    a.classes[words[1]] = (reason.strip(), where)
                elif kind == "allow" and len(words) >= 4 and words[3] in CLASSES:
                    cond = {"with": [], "without": [], "in": []}
                    for w in words[4:]:
                        k, eq, v = w.partition("=")
                        if k not in cond or not eq or not v:
                            raise GateError(f"{where}: bad condition {w!r} (use with=, without= or in=)")
                        cond[k] += [x.strip("/") for x in v.split(",")]
                    a.entries.append({"name": words[1], "version": words[2], "class": words[3], "with": cond["with"],
                                      "without": cond["without"], "in": cond["in"], "reason": reason.strip(),
                                      "where": where, "used": False})
                else:
                    raise GateError(f"{where}: can't parse {line!r}")
                if kind in ("targets", "exclude", "class", "allow") and not reason.strip():
                    raise GateError(f"{where}: every {kind} line needs a reason after ' -- '")
        return a

    def targets_for(self, rel):
        for prefix, triples, _, _ in self.target_overrides:
            if rel == prefix or rel.startswith(prefix + "/"):
                return triples
        return self.targets

    def excluded(self, rel):
        for prefix, reason, where in self.excludes:
            if rel == prefix or rel.startswith(prefix + "/"):
                return (prefix, reason, where)
        return None

    def match(self, name, version, cls, ws):
        for e in self.entries:
            if e["name"] == name and e["class"] == cls and fnmatch.fnmatchcase(version, e["version"]) and \
                    (not e["in"] or any(ws == d or ws.startswith(d + "/") for d in e["in"])):
                return e
        return None


# ---------------------------------------------------------------------------------------------
# cargo tree, parsed into nodes, features and edges

def parse_tree(text):
    """`cargo tree --charset ascii --format "{p}|{f}"` (indent prefix) -> ({(name, ver): features},
    {(name, ver): {(child, kind)}}, [root keys])."""
    feats, edges, roots = {}, {}, []
    stack, section = [], {}
    for raw in text.splitlines():
        line = raw.rstrip()
        if not line:
            stack, section = [], {}
            continue
        i = 0
        while line[i:i + 4] in ("|   ", "    ", "|-- ", "`-- "):
            i += 4
        depth, rest = i // 4, line[i:]
        if rest.startswith("[") and rest.endswith("]"):
            section[depth] = {"[build-dependencies]": "build", "[dev-dependencies]": "dev"}.get(rest, "other")
            continue
        if rest.endswith(" (*)"):
            rest = rest[:-4]
        p, _, f = rest.partition("|")
        parts = p.split(" ")
        if len(parts) < 2 or not parts[1].startswith("v"):
            raise GateError(f"unexpected cargo tree line: {raw!r}")
        key = (parts[0], parts[1][1:])
        feats.setdefault(key, set()).update(x for x in f.split(",") if x)
        edges.setdefault(key, set())
        del stack[depth:]
        for d in [d for d in section if d >= depth]:
            del section[d]
        if depth == 0:
            roots.append(key)
        else:
            if len(stack) < depth:
                raise GateError(f"cargo tree line deeper than its parent: {raw!r}")
            edges[stack[depth - 1]].add((key, section.get(depth - 1, "normal")))
        stack.append(key)
    return feats, edges, roots


def cargo_tree(root, triple, offline, pkgid=None):
    cmd = ["cargo", "tree", "--locked", "-e", "normal,build", "--target", triple, "--charset", "ascii",
           "--format", "{p}|{f}", "--manifest-path", os.path.join(root, "Cargo.toml")]
    cmd += ["-p", pkgid] if pkgid else ["--workspace"]
    if offline:
        cmd.append("--offline")
    return parse_tree(run(cmd, cwd=root))


# ---------------------------------------------------------------------------------------------
# Package facts

_scan_cache = {}


def scan_package(pkg_dir, own):
    """File facts of one package directory (cached; registry sources are immutable)."""
    key = (os.path.normcase(os.path.abspath(pkg_dir)), own)
    if key in _scan_cache:
        return _scan_cache[key]
    srcs, bins, asm, link_attr = {}, 0, 0, 0
    for dirpath, dirnames, filenames in os.walk(pkg_dir):
        keep = []
        for d in dirnames:
            if d in SKIP_DIRS:
                continue
            # In the workspace's own crates, a nested package is its own crate (audited as one if it is
            # built). A registry package is scanned whole: its build script may compile any file in it.
            if own and os.path.isfile(os.path.join(dirpath, d, "Cargo.toml")):
                continue
            keep.append(d)
        dirnames[:] = keep
        in_test_dir = any(part in ("tests", "benches", "examples")
                          for part in os.path.relpath(dirpath, pkg_dir).split(os.sep))
        for fn in filenames:
            ext = os.path.splitext(fn)[1]
            if ext in SRC_EXT:
                srcs[SRC_EXT[ext]] = srcs.get(SRC_EXT[ext], 0) + 1
            elif ext in BIN_EXT:
                bins += 1
            elif ext == ".rs" and not in_test_dir:
                try:
                    with open(os.path.join(dirpath, fn), "rb") as f:
                        text = f.read()
                except OSError:
                    continue
                if any(m in text for m in ASM_MACROS):
                    asm += 1
                if own and LINK_ATTR.search(text):
                    link_attr += 1
    facts = {"srcs": srcs, "bins": bins, "asm": asm, "link_attr": link_attr}
    _scan_cache[key] = facts
    return facts


_links_cache = {}


def build_script_links(pkg):
    """True if the package's build script mentions rustc-link-lib (asks cargo to link a native library)."""
    if pkg["id"] in _links_cache:
        return _links_cache[pkg["id"]]
    _links_cache[pkg["id"]] = False
    for t in pkg["targets"]:
        if "custom-build" in t["kind"]:
            try:
                with open(t["src_path"], "rb") as f:
                    if b"rustc-link-lib" in f.read():
                        _links_cache[pkg["id"]] = True
            except OSError:
                pass
    return _links_cache[pkg["id"]]


def closure(start, edges):
    """Every package reachable from `start` over any edge."""
    seen, todo = set(), list(start)
    while todo:
        k = todo.pop()
        if k in seen:
            continue
        seen.add(k)
        todo.extend(c for c, _ in edges.get(k, ()))
    return seen


def classify(key, pkg, edges, own):
    name = key[0]
    build_script = any("custom-build" in t["kind"] for t in pkg["targets"])
    direct_build = [c for c, kind in edges.get(key, ()) if kind == "build"]
    reach = {k[0] for k in closure(direct_build, edges)}
    native = sorted(reach & NATIVE_BUILD)
    syslib = sorted(reach & SYSLIB_BUILD)
    facts = scan_package(os.path.dirname(pkg["manifest_path"]), own)
    links_lib = build_script and build_script_links(pkg)
    srcs = facts["srcs"]
    if build_script and native and srcs:
        cls = "compiles-native"
    elif build_script and native:
        cls = "native-helper"
    elif build_script and srcs:
        cls = "native-sources"
    elif IMPORTLIB.match(name):
        cls = "windows-import-lib"
    elif build_script and facts["bins"]:
        cls = "prebuilt-native"
    elif pkg.get("links") or syslib or links_lib or (own and facts["link_attr"]):
        cls = "links-native"
    elif facts["asm"]:
        cls = "inline-asm"
    elif srcs:
        cls = "inert-sources"
    else:
        cls = "pure"
    return {"name": name, "version": key[1], "class": cls, "native": native, "syslib": syslib,
            "links": pkg.get("links"), "srcs": srcs, "bins": facts["bins"], "asm": facts["asm"],
            "build_links": links_lib, "link_attr": facts["link_attr"] if own else 0, "own": own,
            "build_script": build_script}


# ---------------------------------------------------------------------------------------------
# One workspace

def metadata(root, offline):
    root = os.path.abspath(root)
    cmd = ["cargo", "metadata", "--format-version", "1", "--locked", "--manifest-path",
           os.path.join(root, "Cargo.toml")]
    if offline:
        cmd.append("--offline")
    return json.loads(run(cmd, cwd=root))


def audit_workspace(root, triples, allow, offline, meta=None, ws="."):
    """-> {triple: {"rows": [...], "problems": [...]}}"""
    root = os.path.abspath(root)
    meta = meta or metadata(root, offline)
    by_key = {}
    for p in meta["packages"]:
        by_key.setdefault((p["name"], p["version"]), []).append(p)
    members = [p for p in meta["packages"] if p["id"] in set(meta["workspace_members"])]
    member_keys = {(p["name"], p["version"]) for p in members}
    out = {}
    for t in triples:
        feats, edges, _ = cargo_tree(root, t, offline)
        rows, problems = [], []
        for key in sorted(feats):
            cands = by_key.get(key)
            if not cands:
                raise GateError(f"{root}: cargo tree lists {key[0]} {key[1]}, cargo metadata doesn't")
            # Two sources with one name and version (rare): classify each, keep the most severe.
            got = [classify(key, p, edges, key in member_keys) for p in cands]
            r = min(got, key=lambda g: CLASSES.index(g["class"]))
            r["features"] = sorted(feats[key])
            rows.append(r)
        if allow:
            judge(rows, allow, problems, ws)
            # Feature conditions hold per crate built on its own (`cargo build -p`), where features are
            # unified over that crate's graph only, not over the whole workspace.
            conditional = {(r["name"], r["version"]): r for r in rows if r.get("entry") and
                           (r["entry"]["with"] or r["entry"]["without"])}
            for m in members:
                mkey = (m["name"], m["version"])
                # A crate built on its own resolves a subset of the workspace's graph, so only the
                # crates that reach a conditional package in the whole graph can build it.
                if not conditional or not closure([mkey], edges) & set(conditional):
                    continue
                mf, _, _ = cargo_tree(root, t, offline, pkgid=m["id"])
                for key, r in conditional.items():
                    if key in mf:
                        check_condition(r, mf[key], f"{m['name']} built on its own", problems)
            for r in rows:
                if r.get("entry") and (r["entry"]["with"] or r["entry"]["without"]):
                    check_condition(r, set(r["features"]), "the whole workspace", problems)
        out[t] = {"rows": rows, "problems": problems}
    return out


def judge(rows, allow, problems, ws):
    for r in rows:
        if r["class"] == "pure":
            r["verdict"] = "pure"
            continue
        e = allow.match(r["name"], r["version"], r["class"], ws)
        if e:
            e["used"] = True
            r["entry"], r["verdict"] = e, f"accepted: allow line {lineno(e['where'])}"
        elif r["class"] in allow.classes:
            r["verdict"] = f"accepted: class line {lineno(allow.classes[r['class']][1])}"
        else:
            r["verdict"] = "NOT ACCEPTED"
            problems.append(f"{r['name']} {r['version']}: {r['class']}{' (own crate)' if r['own'] else ''} "
                            f"{describe(r)} is not accepted by the allow file")


def lineno(where):
    return where.rsplit(":", 1)[-1]


def check_condition(r, features, where, problems):
    e = r["entry"]
    missing = [f for f in e["with"] if f not in features]
    present = [f for f in e["without"] if f in features]
    if missing or present:
        r["verdict"] = "NOT ACCEPTED (condition)"
        bits = ([f"needs feature(s) {', '.join(missing)}"] if missing else []) + \
               ([f"must not have feature(s) {', '.join(present)}"] if present else [])
        problems.append(f"{r['name']} {r['version']}: built by {where} with features [{', '.join(sorted(features))}]; "
                        f"{e['where']} accepts it only when it {' and '.join(bits)}")


def describe(r):
    bits = []
    if r["native"]:
        bits.append("helpers " + ", ".join(r["native"]))
    if r["syslib"]:
        bits.append("system-library finders " + ", ".join(r["syslib"]))
    if r["links"]:
        bits.append(f"links={r['links']}")
    if r["srcs"]:
        bits.append("sources " + ", ".join(f"{k} {v}" for k, v in sorted(r["srcs"].items())))
    if r["bins"]:
        bits.append(f"{r['bins']} prebuilt native files")
    if r["build_links"]:
        bits.append("build script emits rustc-link-lib")
    if r["link_attr"]:
        bits.append(f"{r['link_attr']} files with #[link]")
    if r["asm"]:
        bits.append(f"{r['asm']} files with inline asm")
    return "(" + "; ".join(bits) + ")" if bits else ""


# ---------------------------------------------------------------------------------------------
# Discovery (check mode)

WS_RE = re.compile(r"(?m)^\s*\[workspace\]")
PKG_RE = re.compile(r"(?m)^\s*\[package\]")


def list_manifests(tree):
    if os.path.exists(os.path.join(tree, ".git")):
        out = run(["git", "-C", tree, "ls-files", "-z", "-c", "-o", "--exclude-standard"])
        rels = [p for p in out.split("\0") if p and (p == "Cargo.toml" or p.endswith("/Cargo.toml"))]
        return sorted(p for p in rels if os.path.isfile(os.path.join(tree, p)))
    rels = []
    for dirpath, dirnames, filenames in os.walk(tree):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        if "Cargo.toml" in filenames:
            rels.append(os.path.relpath(os.path.join(dirpath, "Cargo.toml"), tree).replace(os.sep, "/"))
    return sorted(rels)


def rel_of(tree, path):
    return os.path.normcase(os.path.relpath(os.path.normpath(path), os.path.normpath(tree))).replace(os.sep, "/")


def check(tree, allow, offline, md):
    manifests = list_manifests(tree)
    if not manifests:
        raise GateError(f"no Cargo.toml under {tree}")
    info = {}
    for rel in manifests:
        with open(os.path.join(tree, rel), encoding="utf-8", errors="replace") as f:
            text = f.read()
        d = os.path.dirname(rel)
        info[rel] = {"ws": bool(WS_RE.search(text)), "pkg": bool(PKG_RE.search(text)),
                     "lock": os.path.isfile(os.path.join(tree, d, "Cargo.lock")), "dir": d or "."}
    roots = [r for r in manifests if info[r]["ws"] or (info[r]["pkg"] and info[r]["lock"])]
    problems, covered, results, skipped = [], set(), [], []
    for rel in roots:
        d = info[rel]["dir"]
        ex = allow.excluded(d)
        if ex:
            skipped.append((d, ex))
            continue
        root = os.path.join(tree, d)
        if not info[rel]["lock"]:
            problems.append(f"{d}: a workspace without a committed Cargo.lock (the gate audits locked graphs)")
            continue
        meta = metadata(root, offline)
        for p in meta["packages"]:
            if p["source"] is None:
                covered.add(rel_of(tree, p["manifest_path"]))
        triples = allow.targets_for(d)
        res = audit_workspace(root, triples, allow, offline, meta=meta, ws=d)
        results.append((d, triples, res))
        for t, r in res.items():
            problems += [f"{d} [{t}]: {p}" for p in r["problems"]]
    for rel in manifests:
        if info[rel]["pkg"] and os.path.normcase(rel).replace(os.sep, "/") not in covered \
                and not allow.excluded(info[rel]["dir"]):
            problems.append(f"{rel}: a crate in no workspace the gate audits; make it a member of one (or give it "
                            f"its own workspace with a Cargo.lock), or add an exclude line with a reason")
    unused = [e for e in allow.entries if not e["used"]]
    write_report(md, tree, results, skipped, problems, unused, allow, check_mode=True)
    for d, triples, res in results:
        for t, r in res.items():
            rows = r["rows"]
            flagged = [x for x in rows if x["class"] != "pure"]
            print(f"audit_native: {d} [{t}]: {len(rows)} packages, {len(rows) - len(flagged)} pure Rust, "
                  f"{len(flagged)} flagged, {len(r['problems'])} not accepted")
    for d, (prefix, reason, where) in skipped:
        print(f"audit_native: skipped {d} (exclude line {lineno(where)})")
    for e in unused:
        print(f"audit_native: note: allow line {lineno(e['where'])} ({e['name']} {e['class']}) matched no package")
    if problems:
        for p in problems:
            print(f"audit_native: FAIL: {p}")
        print(f"audit_native: FAIL, {len(problems)} problem(s); see the allow file's header for how to accept a "
              f"reviewed exception")
        return 1
    n = sum(len(r["rows"]) for _, _, res in results for r in res.values())
    print(f"audit_native: PASS, 100% Rust under D1: {len(results)} workspace(s), {n} package builds checked, "
          f"every flag accepted by the allow file")
    return 0


# ---------------------------------------------------------------------------------------------
# Report

def write_report(md, tree, results, skipped, problems, unused, allow, check_mode):
    if not md:
        return
    L = ["# Native-code audit", "",
         "Generated by `scripts/audit_native.py`" + (" --check (the landing gate)" if check_mode else "") +
         ": the packages `cargo tree -e normal,build --target <triple>` builds, classified from `cargo tree` "
         "edges and `cargo metadata` facts (classes: the script's docstring). Accepted means the allow file "
         "(`scripts/native_allow.txt`) accepts the package or its class, with a reason.", ""]
    if check_mode:
        L += [f"**{'FAIL' if problems else 'PASS'}**: " +
              (f"{len(problems)} problem(s)." if problems else "100% Rust under D1; every flag is accepted."), ""]
        for p in problems:
            L.append(f"* {p}")
        if problems:
            L.append("")
    for d, triples, res in results:
        L += [f"## {d}", ""]
        for t in triples:
            rows = res[t]["rows"]
            flagged = [r for r in rows if r["class"] != "pure"]
            own = sum(1 for r in rows if r["own"])
            L += [f"**{t}**: {len(rows)} packages ({own} of them this workspace's own), "
                  f"{len(rows) - len(flagged)} pure Rust, {len(flagged)} flagged", ""]
            if flagged:
                L += ["| package | class | details | features | verdict |", "|---|---|---|---|---|"]
                for r in flagged:
                    L.append(f"| {r['name']} {r['version']}{' (own)' if r['own'] else ''} | {r['class']} | "
                             f"{describe(r).strip('()')} | {', '.join(r['features'])} | {r.get('verdict', '')} |")
                L.append("")
    used = [e for e in allow.entries if e["used"]] if allow else []
    if used or (allow and allow.classes):
        L += ["## Why each accepted flag is accepted (the allow file's lines)", ""]
        for c, (reason, where) in sorted(allow.classes.items(), key=lambda x: int(lineno(x[1][1]))):
            L.append(f"* class line {lineno(where)}, `{c}`: {reason}")
        for e in used:
            cond = " ".join([f"with={','.join(e['with'])}"] * bool(e["with"]) +
                            [f"without={','.join(e['without'])}"] * bool(e["without"]) +
                            [f"in={','.join(e['in'])}"] * bool(e["in"]))
            L.append(f"* allow line {lineno(e['where'])}, `{e['name']} {e['version']} {e['class']}"
                     f"{' ' + cond if cond else ''}`: {e['reason']}")
        L.append("")
    if skipped:
        L += ["## Not audited (exclude lines)", ""]
        for d, (prefix, reason, where) in skipped:
            L.append(f"* `{d}`: {reason} (exclude line {lineno(where)})")
        L.append("")
    if unused:
        L += ["## Allow lines that matched nothing", ""]
        for e in unused:
            L.append(f"* allow line {lineno(e['where'])}: {e['name']} {e['version']} {e['class']}")
        L.append("")
    with open(md, "w", newline="\n", encoding="utf-8") as f:
        f.write("\n".join(L) + "\n")


def report(dirs, triples, allow, offline, md):
    results = []
    for d in dirs:
        res = audit_workspace(d, triples, allow, offline)
        results.append((os.path.basename(os.path.normpath(d)) or d, triples, res))
        for t, r in res.items():
            rows = r["rows"]
            flagged = [x for x in rows if x["class"] != "pure"]
            print(f"{d} [{t}]: {len(rows)} packages, {len(rows) - len(flagged)} pure Rust, {len(flagged)} flagged")
            for x in flagged:
                print(f"    {x['name']} {x['version']}: {x['class']} {describe(x)} {x.get('verdict', '')}")
    write_report(md, ".", results, [], [], [], allow, check_mode=False)
    return 0


def main():
    args = sys.argv[1:]
    triples, dirs, md, allow_path, tree, mode, offline = [], [], None, None, None, "report", False
    i = 0
    try:
        while i < len(args):
            a = args[i]
            if a in ("--target", "--md", "--allow", "--tree"):
                if i + 1 >= len(args):
                    raise GateError(f"{a} needs a value")
                v = args[i + 1]
                i += 2
                if a == "--target":
                    triples.append(v)
                elif a == "--md":
                    md = v
                elif a == "--allow":
                    allow_path = v
                else:
                    tree = v
                continue
            if a == "--check":
                mode = "check"
            elif a == "--offline":
                offline = True
            elif a.startswith("-"):
                raise GateError(f"unknown option {a}")
            else:
                dirs.append(a)
            i += 1
        if mode == "check":
            if dirs or triples:
                raise GateError("--check finds the workspaces and takes the targets from the allow file")
            tree = os.path.abspath(tree or ".")
            allow = Allow.parse(allow_path or os.path.join(tree, "scripts", "native_allow.txt"))
            if not allow.targets:
                raise GateError("the allow file names no target lines")
            return check(tree, allow, offline, md)
        if not dirs:
            raise GateError("usage: audit_native.py <workspace-dir>... [--target T]... [--md OUT] [--allow FILE] "
                            "| --check [--tree DIR] [--allow FILE] [--md OUT]")
        allow = Allow.parse(allow_path) if allow_path else None
        return report(dirs, triples or (allow.targets if allow else ["x86_64-pc-windows-msvc"]), allow, offline, md)
    except GateError as e:
        print(f"audit_native: ERROR: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
