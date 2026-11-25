#!/usr/bin/env python3
"""Apply the native-code gate's classifier to the opt-in Steam Audio build."""

import argparse
import importlib.util
import json
from pathlib import Path
import sys

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("audit_native", ROOT / "scripts" / "audit_native.py")
audit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit)

TARGETS = ("x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu")


def inspect_target(root, target, offline, allow):
    cargo_options = ["--locked"] + (["--offline"] if offline else [])
    metadata = json.loads(audit.run([
        "cargo", "metadata", *cargo_options, "--format-version", "1",
        "--features", "pe-app/steam-audio-trial", "--filter-platform", target,
    ], cwd=root))
    members = set(metadata["workspace_members"])
    packages = {(package["name"], package["version"]): package for package in metadata["packages"]}
    tree = audit.run([
        "cargo", "tree", *cargo_options, "-p", "pe-app", "--features", "steam-audio-trial",
        "-e", "normal,build", "--target", target, "--charset", "ascii", "--format", "{p}|{f}",
    ], cwd=root)
    features, edges, _ = audit.parse_tree(tree)
    if ("cpal", "0.18.2") not in features:
        raise audit.GateError(f"{target}: expected CPAL 0.18.2 with the trial enabled")
    rows = [
        audit.classify(key, packages[key], edges, packages[key]["id"] in members)
        for key in sorted(features)
    ]
    problems = []
    audit.judge(rows, allow, problems, ".")
    for row in rows:
        resolved = features[(row["name"], row["version"])]
        if "entry" in row:
            audit.check_condition(row, resolved, "pe-app with steam-audio-trial", problems)
        if row["name"] == "alsa-sys":
            print(f"{target}: alsa-sys {row['version']} {row['class']} features={sorted(resolved)} {row['verdict']}")
    print(f"{target}: {len(rows)} packages, {len(problems)} rejected")
    for problem in problems:
        print(problem)
    return bool(problems)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    try:
        allow = audit.Allow.parse(ROOT / "scripts" / "native_allow.txt")
        allow.entries.extend(audit.Allow.parse(ROOT / "scripts" / "native_allow_steam_audio.txt").entries)
        failed = [inspect_target(ROOT, target, args.offline, allow) for target in TARGETS]
    except audit.GateError as error:
        print(error, file=sys.stderr)
        return 2
    return int(any(failed))


if __name__ == "__main__":
    raise SystemExit(main())
