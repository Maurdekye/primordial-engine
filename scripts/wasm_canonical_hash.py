"""Canonical content hash of a wasm module (design 13.2, review M8).

The commit's wasm identity is SHA-256 over the module with every custom section removed (id 0:
`name`, `producers`, debug info), i.e. only the sections the runtime executes. Builds of the same
source from different checkout paths differ only in custom sections (mangled crate hashes in
`name` come from the path-dependent -C metadata).

usage: python -I scripts/wasm_canonical_hash.py FILE.wasm [FILE.wasm ...]
"""
import hashlib
import sys


def leb(b, i):
    r = s = 0
    while True:
        x = b[i]
        i += 1
        r |= (x & 0x7F) << s
        s += 7
        if x < 0x80:
            return r, i


def canonical(b):
    assert b[:4] == b"\0asm", "not a wasm module"
    out, i, dropped = bytearray(b[:8]), 8, []
    while i < len(b):
        sid = b[i]
        n, j = leb(b, i + 1)
        if sid == 0:
            name_len, k = leb(b, j)
            dropped.append(bytes(b[k:k + name_len]).decode("utf-8", "replace"))
        else:
            out += b[i:j + n]
        i = j + n
    return bytes(out), dropped


for path in sys.argv[1:]:
    raw = open(path, "rb").read()
    can, dropped = canonical(raw)
    print(f"{path}: raw sha256 {hashlib.sha256(raw).hexdigest()[:16]} ({len(raw)} B) | "
          f"canonical sha256 {hashlib.sha256(can).hexdigest()[:16]} ({len(can)} B) | custom sections removed: {dropped}")
