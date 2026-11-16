"""Cargo output roots for developer scripts; never share a target between source trees."""
import hashlib
import os
from pathlib import Path
import re
import sys


def target_dir(source, leaf=""):
    source = Path(source).resolve()
    root = os.environ.get("PE_TARGET_ROOT")
    if not root and os.name == "nt" and Path("<work>").is_dir():
        root = "<work>/target"
    if root:
        key = re.sub(r"[^A-Za-z0-9_.-]", "-", source.name) + "-" + hashlib.sha256(
            str(source).replace("\\", "/").lower().encode()).hexdigest()[:10]
        return str(Path(root) / key / leaf)
    return str(source / "target" / leaf)


if __name__ == "__main__":
    print(target_dir(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else ""))
