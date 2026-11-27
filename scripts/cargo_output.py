"""Cargo output roots for developer scripts; never share a target between source trees."""
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys


def target_dir(source, leaf=""):
    source = Path(source).resolve()
    root = os.environ.get("PE_TARGET_ROOT")
    if not root:
        # A machine-wide root for this clone: git config pe.targetroot (local config, shared by worktrees).
        r = subprocess.run(["git", "-C", str(source), "config", "--get", "pe.targetroot"],
                           capture_output=True, text=True)
        root = r.stdout.strip() if r.returncode == 0 else None
    if root:
        key = re.sub(r"[^A-Za-z0-9_.-]", "-", source.name) + "-" + hashlib.sha256(
            str(source).replace("\\", "/").lower().encode()).hexdigest()[:10]
        return str(Path(root) / key / leaf)
    return str(source / "target" / leaf)


if __name__ == "__main__":
    print(target_dir(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else ""))
