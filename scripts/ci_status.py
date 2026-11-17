"""Tier 2's state (docs/refound-plan.md 4.6, decision C7): the hosted runs of .github/workflows/gate.yml on the
public repository's main. docs/landing-gates.md describes tier 2.

usage: python -I scripts/ci_status.py [--repo OWNER/NAME] [--check FILE | --new STATEFILE | --run ID]
  (no option)        the latest finished runs on main, with the failed jobs of a red one.
  --check FILE       the landing gate's C7 step (scripts/gate.sh, step 5). Exit 1 if tier 2 is red: the latest
                     finished run on main that gives a verdict is red, and its id isn't listed in FILE
                     (scripts/tier2_explained.txt as the landing has it, `<run id> -- <reason>`). Exit 0 if that run
                     is green or explained, or there is none yet; 3 if GitHub can't be read.
  --new STATEFILE    for the triage watchdog: one `TIER2 RED <run id> <conclusion> <url> jobs: <failed jobs>` line
                     per red run not seen before (the ids seen are kept in STATEFILE), and nothing otherwise.
  --run ID           for triage: that run's failed jobs, their failed steps and their error annotations. The
                     workflow writes the failing tests and any differing digest there, because annotations are
                     public and logs are not.
Only main's own runs count: a push, the nightly schedule or a manual dispatch, in the repository itself. A pull
request's run never does, not even one whose branch is a fork's main: GitHub's branch filter matches a run's head
branch, whichever repository it comes from (chunks-reviewer's M1).
A finished run is green when its conclusion is success or neutral, and red when it is failure, timed_out or
startup_failure. Any other (cancelled, skipped, stale: a burst of pushes cancels the runs still waiting) gives no
verdict, and the check looks past it to the run before.
It reads GitHub's public API without credentials (60 requests an hour per address): one call, plus one for a red
run's jobs. --check, --new and the listing give up after 20 s in all, whatever holds them up (a DNS lookup, a server
that trickles bytes), because the landing hook runs --check while git holds main's locks; --run gives up after
60 s. A 404 on the listing means the workflow has no run yet. PE_CI_RUNS_URL replaces the listing's URL (the
self-test points it at a fixture, or at a server that never finishes its answer).
Exit 2 on a usage error, 4 if this script fails in any other way, so that a crash never reads as a red run.
"""

import json
import os
import sys
import threading
import urllib.error
import urllib.request

REPO = "Maurdekye/primordial-engine"
API = "https://api.github.com/repos/{repo}/actions/workflows/gate.yml/runs?branch=main&status=completed&per_page=50"
OWN_EVENTS = ("push", "schedule", "workflow_dispatch")
LIMIT_S = 20
TRIAGE_LIMIT_S = 60
GREEN = ("success", "neutral")
RED = ("failure", "timed_out", "startup_failure")


class Unreadable(Exception):
    pass


def get(url, missing_ok=False):
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json",
                                               "User-Agent": "primordial-engine-ci-status"})
    try:
        with urllib.request.urlopen(req, timeout=LIMIT_S) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        if missing_ok and e.code == 404:
            return None
        raise Unreadable(f"HTTP {e.code} from {url.split('?')[0]}") from None
    except (urllib.error.URLError, OSError, ValueError) as e:
        raise Unreadable(str(e)) from None


def paged(url):
    return url if url.startswith("file:") else url + ("&" if "?" in url else "?") + "per_page=50"


def runs(repo):
    """Main's own finished runs, newest first: never a pull request's, whatever its branch is called."""
    data = get(os.environ.get("PE_CI_RUNS_URL") or API.format(repo=repo), missing_ok=True)
    return [r for r in (data or {}).get("workflow_runs", [])
            if r.get("event") in OWN_EVENTS
            and str((r.get("head_repository") or {}).get("full_name", "")).lower() == repo.lower()]


def failed_jobs(run):
    """The names of a finished run's jobs that didn't pass; a list with one note if they can't be read."""
    try:
        jobs = get(paged(run["jobs_url"])).get("jobs", [])
    except Unreadable:
        return ["(jobs unreadable)"]
    return [j["name"] for j in jobs if j.get("conclusion") not in ("success", "skipped", "neutral", None)]


def explained(path):
    ids = set()
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line and not line.startswith("#"):
                    ids.add(line.split()[0])
    except FileNotFoundError:
        pass
    return ids


def check(repo, path):
    """(exit code, message) for the gate."""
    rs = runs(repo)
    for n, r in enumerate(rs):
        c = r.get("conclusion")
        past = f" (past {n} run(s) with no verdict)" if n else ""
        if c in GREEN:
            return 0, f"tier 2 green: run {r['id']} on {r['head_sha'][:10]}{past}"
        if c in RED:
            if str(r["id"]) in explained(path):
                return 0, f"tier 2 {c} in run {r['id']}, explained in {os.path.basename(path)}{past}"
            jobs = ", ".join(failed_jobs(r)) or "(none named)"
            return 1, f"tier 2 RED: run {r['id']} ({c}) on {r['head_sha'][:10]}, jobs {jobs}: {r['html_url']}{past}"
    if not rs:
        return 0, "no finished tier-2 run on main yet"
    seen = ", ".join(sorted({str(r.get("conclusion")) for r in rs}))
    return 0, f"no verdict among the last {len(rs)} finished runs on main ({seen})"


def fresh_reds(repo, state):
    """(lines to print, run ids to remember) for the triage watchdog."""
    try:
        with open(state, encoding="utf-8") as f:
            seen = set(f.read().split())
    except FileNotFoundError:
        seen = set()
    rs = runs(repo)
    lines = [f"TIER2 RED {r['id']} {r['conclusion']} {r['html_url']} jobs: {', '.join(failed_jobs(r))}"
             for r in reversed(rs) if r.get("conclusion") in RED and str(r["id"]) not in seen]
    return lines, [str(r["id"]) for r in rs if str(r["id"]) not in seen]


def triage(repo, run_id):
    run = get(f"https://api.github.com/repos/{repo}/actions/runs/{run_id}")
    out = [f"run {run['id']} {run['event']} {run['head_sha'][:10]} {run['conclusion']}: {run['html_url']}"]
    for j in get(paged(run["jobs_url"])).get("jobs", []):
        if j.get("conclusion") in ("success", "skipped", "neutral"):
            continue
        steps = [s["name"] for s in j.get("steps", []) if s.get("conclusion") not in ("success", "skipped", None)]
        out.append(f"  job {j['name']}: {j.get('conclusion')}; failed steps: {', '.join(steps) or '-'}")
        for a in get(paged(j["check_run_url"] + "/annotations")):
            msg = (a.get("message") or "").strip()[:4000].replace(chr(10), chr(10) + " " * 6)
            out.append(f"    {a.get('annotation_level')}: {a.get('title') or ''}: {msg}")
    return out


def listing(repo):
    out = []
    for r in runs(repo)[:5]:
        line = f"{r['created_at']} run {r['id']} {r['event']:<8} {r['head_sha'][:10]} {r['conclusion']}"
        if r.get("conclusion") in RED:
            line += f"  failed: {', '.join(failed_jobs(r))}  {r['html_url']}"
        out.append(line)
    return out or ["no finished run on main yet"]


def bounded(limit, fn, *args):
    """fn(*args) in a daemon thread, given `limit` seconds in all. On time-out it reports and exits 3 at once:
    a socket's own time-out bounds each read, not a DNS lookup or a server that sends a byte now and then."""
    box = {}

    def work():
        try:
            box["ok"] = fn(*args)
        except BaseException as e:  # handed to the main thread
            box["err"] = e

    t = threading.Thread(target=work, daemon=True)
    t.start()
    t.join(limit)
    if t.is_alive():
        print(f"ci_status: GitHub can't be read: no answer within {limit} s", flush=True)
        os._exit(3)
    if "err" in box:
        raise box["err"]
    return box["ok"]


def main():
    args, repo, mode, value = sys.argv[1:], os.environ.get("PE_CI_REPO", REPO), None, None
    while args:
        a = args.pop(0)
        if a == "--repo" and args:
            repo = args.pop(0)
        elif a in ("--check", "--new", "--run") and args and mode is None:
            mode, value = a, args.pop(0)
        else:
            print(__doc__)
            return 2
    try:
        if mode == "--check":
            code, msg = bounded(LIMIT_S, check, repo, value)
            print(f"ci_status: {msg}")
            return code
        if mode == "--new":
            lines, ids = bounded(LIMIT_S, fresh_reds, repo, value)
            for line in lines:
                print(line)
            if ids:
                with open(value, "a", encoding="utf-8") as f:
                    f.write("".join(i + "\n" for i in ids))
            return 0
        if mode == "--run":
            lines = bounded(TRIAGE_LIMIT_S, triage, repo, int(value))
        else:
            lines = bounded(LIMIT_S, listing, repo)
        print("\n".join(lines))
        return 0
    except Unreadable as e:
        print(f"ci_status: GitHub can't be read ({e})")
        return 3


if __name__ == "__main__":
    try:
        rc = main()
    except BaseException as e:  # a crash must not exit 1, which means red
        print(f"ci_status: failed: {type(e).__name__}: {e}")
        rc = 4
    sys.stdout.flush()
    sys.exit(rc)
