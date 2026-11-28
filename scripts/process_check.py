"""Process check: the team's mail ids stay out of the repository (user ruling 2026-11-27: the agents'
workspaces are not part of the engine). A mail id is `m` and 12 lowercase hex digits, standing alone.
Provenance (who ruled what, in which mail) belongs to the docket and the team's notes; the repository says
what changed and why. It also refuses a bare CR (a CR without its LF) in a text file: git stores text with LF
(.gitattributes), and a publish_apply before its CRLF fix left one where a rule joined two CRLF lines.

usage: python -I scripts/process_check.py <rev> [<range>]
  <rev>    every text file of its tree (a NUL in the first 8 KiB marks a binary file, which is skipped)
  <range>  also the messages of those commits (git log revision arguments, e.g. base..rev)
Run in the repository (or with GIT_DIR set). The other process terms (scratch paths, this PC's drive
paths, host names) are publication terms: scripts/publish_check.sh checks them with the local term file.
Exit 0 when nothing is found, 1 with each hit printed, 2 on an error.
"""
import re
import subprocess
import sys

MAIL_ID = re.compile(rb"(?<![0-9A-Za-z])m[0-9a-f]{12}(?![0-9A-Za-z])")
BARE_CR = re.compile(rb"\r(?!\n)")


def git(*args):
    r = subprocess.run(["git", *args], capture_output=True)
    if r.returncode != 0:
        print(f"process_check: git {' '.join(args[:2])} failed: {r.stderr.decode('utf-8', 'replace').strip()}")
        sys.exit(2)
    return r.stdout


def main():
    if len(sys.argv) not in (2, 3):
        print(__doc__)
        sys.exit(2)
    rev = sys.argv[1]
    hits = []
    listing = git("ls-tree", "-r", "-z", "--full-tree", rev).split(b"\0")
    blobs = []
    for entry in listing:
        if not entry:
            continue
        meta, path = entry.split(b"\t", 1)
        mode, kind, sha = meta.split()
        if kind == b"blob" and mode != b"120000":
            blobs.append((sha.decode(), path.decode("utf-8", "replace")))
    p = subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    for sha, path in blobs:
        p.stdin.write(sha.encode() + b"\n")
        p.stdin.flush()
        size = int(p.stdout.readline().split()[2])
        data = p.stdout.read(size)
        p.stdout.read(1)
        if b"\0" in data[:8192]:
            continue
        for m in MAIL_ID.finditer(data):
            line = data.count(b"\n", 0, m.start()) + 1
            hits.append(f"{path}:{line}: a mail id")
        # A CR without its LF: what an older publish_apply left when a rule joined two CRLF lines.
        for m in BARE_CR.finditer(data):
            line = data.count(b"\n", 0, m.start()) + 1
            hits.append(f"{path}:{line}: a bare CR (a CR without its LF: join the line with a space)")
    p.stdin.close()
    p.wait()
    n_msgs = 0
    if len(sys.argv) == 3:
        log = git("log", "--format=%H%x00%B%x01", sys.argv[2])
        for record in log.split(b"\x01"):
            record = record.strip(b"\n")
            if not record:
                continue
            n_msgs += 1
            commit, body = record.split(b"\0", 1)
            if MAIL_ID.search(body):
                hits.append(f"commit {commit[:10].decode()}: a mail id in its message")
    for h in hits:
        print(h)
    scope = f"{len(blobs)} files" + (f" and {n_msgs} commit messages" if len(sys.argv) == 3 else "")
    if hits:
        print(f"process_check: FAIL: {len(hits)} hit(s) in {scope}")
        sys.exit(1)
    print(f"process_check: PASS: no mail id or bare CR in {scope}")


if __name__ == "__main__":
    main()
