"""Tier 2's state (docs/refound-plan.md 4.6, decision C7): the hosted runs of .github/workflows/gate.yml on the
public repository's main. docs/landing-gates.md describes tier 2.

usage: python -I scripts/ci_status.py [--repo OWNER/NAME] [--check FILE | --new STATEFILE | --run ID]
  (no option)        the latest finished runs on main, each with its verdict.
  --check FILE       the landing gate's C7 step (scripts/gate.sh, step 5). Exit 1 if tier 2 is red: the latest
                     finished run on main that gives a verdict is red, and its id isn't listed in FILE
                     (scripts/tier2_explained.txt as the landing has it, `<run id> -- <reason>`). Exit 0 if that run
                     is green or explained, or there is none yet; 3 if tier 2's state is unknown: GitHub can't be
                     read, or none of the 5 newest runs it judges gives a verdict (it reads no further: chunks-
                     reviewer's L1; a stretch of runs a platform missed would cost a call each).
  --new STATEFILE    for the triage watchdog, nothing when all is well, otherwise:
                     `TIER2 RED <run id> <conclusion> <url> jobs: <red jobs>`, once per red run, followed by
                     `; <platform> unverified since <sha>, up to <sha>` for each platform that is back after runs
                     without a runner; and `TIER2 STALE <platform> unverified since <sha>: ...`, once per stretch,
                     when a platform has had no real verdict for 24 h or for 3 runs in a row. STATEFILE keeps the
                     run ids seen; STATEFILE.platforms.json keeps each platform's last real verdict and its misses.
  --run ID           for triage: that run's verdict, the jobs that didn't pass, their failed steps and their
                     annotations. The workflow writes the failing tests, their panic messages and any differing
                     digest there, because annotations are public and logs are not.
Only main's own runs count: a push, the nightly schedule or a manual dispatch, in the repository itself. A pull
request's run never does, not even one whose branch is a fork's main: GitHub's branch filter matches a run's head
branch, whichever repository it comes from (chunks-reviewer's M1).
A finished run's jobs give its verdict (coordinator m4445bf31f965). It is red when a job failed or timed out, or
started and was then cancelled: GitHub reports a job stopped at its time limit as cancelled, like one cancelled by
hand. Failing that, a job GitHub never gave a runner leaves the run with no verdict: that platform is "not run"
(GitHub had no capacity for it: "not acquired"), and its result is unknown, neither red nor green. So does a run
cancelled before any job started (a burst of pushes cancels the runs still waiting). Failing both, the run's own
conclusion decides: success or neutral is green; failure, timed_out or startup_failure is red; anything else gives
no verdict. The check looks past a run with no verdict to the run before.
It reads GitHub's public API without credentials (60 requests an hour per address): the listing, then each judged
run's jobs (two calls in all, usually; --new judges at most 5 new runs a call). --check, --new and the listing give
up after 20 s in all, whatever holds them up (a DNS lookup, a server that trickles bytes), because the landing hook
runs --check while git holds main's locks; --run gives up after 60 s. A 404 on the listing means the workflow has
no run yet. PE_CI_RUNS_URL replaces the listing's URL (the self-test points it at a fixture, or at a server that
never finishes its answer), and PE_CI_NOW the time --new measures 24 h from.
Exit 2 on a usage error, 4 if this script fails in any other way, so that a crash never reads as a red run.
"""

import json
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone

REPO = "Maurdekye/primordial-engine"
API = "https://api.github.com/repos/{repo}/actions/workflows/gate.yml/runs?branch=main&status=completed&per_page=50"
OWN_EVENTS = ("push", "schedule", "workflow_dispatch")
LIMIT_S = 20
TRIAGE_LIMIT_S = 60
GREEN = ("success", "neutral")
RED = ("failure", "timed_out", "startup_failure")
JUDGED = GREEN + RED + ("cancelled",)  # the conclusions whose jobs are read; any other gives no verdict
PLATFORM = re.compile(r"^tier 2, ([a-z0-9]+-[a-z0-9]+)$")  # a tier-2 job's name, "tier 2, macos-arm64"
STALE_RUNS = 3
STALE_S = 24 * 3600
NEW_AT_MOST = 5  # new runs --new judges a call; older new ones are only remembered
CHECK_AT_MOST = 5  # runs --check judges (a call each) before it calls tier 2's state unknown


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


def jobs_of(run):
    return get(paged(run["jobs_url"])).get("jobs", [])


def started(job):
    """Whether GitHub gave the job a runner. A job it never placed has no runner and no steps."""
    return bool(job.get("runner_name") or job.get("steps"))


def unplaced(job):
    return job.get("conclusion") == "cancelled" and not started(job)


def judge(run, jobs):
    """(verdict, job names): "red" with the red jobs, None with the jobs never given a runner (or none), or
    "green". The module's notes give the rule."""
    red = [j["name"] for j in jobs
           if j.get("conclusion") in RED or (j.get("conclusion") == "cancelled" and started(j))]
    if red:
        return "red", red
    missing = [j["name"] for j in jobs if unplaced(j)]
    if missing:
        return None, missing
    c = run.get("conclusion")
    return ("green" if c in GREEN else "red" if c in RED else None), []


def verdict(run):
    return judge(run, jobs_of(run)) if run.get("conclusion") in JUDGED else (None, [])


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


def not_run(names):
    return ", ".join(PLATFORM.sub(r"\1", n) for n in names)


def check(repo, path):
    """(exit code, message) for the gate."""
    rs = runs(repo)
    gaps, judged = [], 0
    for n, r in enumerate(rs):
        if r.get("conclusion") in JUDGED:
            if judged == CHECK_AT_MOST:
                return 3, (f"no verdict in the {judged} newest runs judged"
                           f"{' (not run: ' + '; '.join(gaps) + ')' if gaps else ''}; older runs not read")
            judged += 1
        v, names = verdict(r)
        past = f" (past {n} run(s) with no verdict{'; not run: ' + '; '.join(gaps) if gaps else ''})" if n else ""
        if v == "green":
            return 0, f"tier 2 green: run {r['id']} on {r['head_sha'][:10]}{past}"
        if v == "red":
            if str(r["id"]) in explained(path):
                where = os.path.basename(path)
                return 0, f"tier 2 {r.get('conclusion')} in run {r['id']}, explained in {where}{past}"
            jobs = ", ".join(names) or "(none named)"
            return 1, (f"tier 2 RED: run {r['id']} ({r.get('conclusion')}) on {r['head_sha'][:10]}, jobs {jobs}: "
                       f"{r['html_url']}{past}")
        if names:
            gaps.append(f"{not_run(names)} in run {r['id']}")
    if not rs:
        return 0, "no finished tier-2 run on main yet"
    seen = ", ".join(sorted({str(r.get("conclusion")) for r in rs}))
    return 0, f"no verdict among the last {len(rs)} finished runs on main ({seen})"


def when(stamp):
    return datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc).timestamp()


def news(repo, state, now):
    """(lines to print, run ids to remember, platform state to keep) for the triage watchdog. Each platform's
    state: its last real verdict ({id, sha, at}, or None before its first), the runs in a row since then that
    GitHub gave it no runner ("misses", the first one's time "since"), and whether that stretch was reported."""
    try:
        with open(state, encoding="utf-8") as f:
            seen = set(f.read().split())
    except FileNotFoundError:
        seen = set()
    lines = []
    try:
        with open(state + ".platforms.json", encoding="utf-8") as f:
            plat = json.load(f)
    except FileNotFoundError:
        plat = {}
    except ValueError:  # not JSON: start it afresh rather than stop the watchdog
        lines.append(f"ci_status: note: {state}.platforms.json was unreadable; it starts afresh")
        plat = {}
    new = [r for r in runs(repo) if str(r["id"]) not in seen]
    for r in reversed(new[:NEW_AT_MOST]):  # oldest first; older new runs are only remembered (the rate limit)
        jobs = jobs_of(r) if r.get("conclusion") in JUDGED else []
        v, names = judge(r, jobs)
        back = []
        for j in jobs:
            m = PLATFORM.match(j.get("name", ""))
            if not m:
                continue
            p = plat.setdefault(m.group(1), {"real": None, "misses": 0, "since": None, "reported": False})
            if unplaced(j):
                p["misses"] += 1
                p["since"] = p["since"] or r["created_at"]
            elif started(j):
                if p["misses"] and v == "red":
                    last = (p["real"] or {}).get("sha", "")[:10] or "tier 2 began"
                    back.append(f"{m.group(1)} unverified since {last}, up to {r['head_sha'][:10]}")
                p.update(real={"id": r["id"], "sha": r["head_sha"], "at": r["created_at"]}, misses=0, since=None,
                         reported=False)
        if v == "red":
            jobs_named = ", ".join(names) or "(none named)"
            lines.append(f"TIER2 RED {r['id']} {r['conclusion']} {r['html_url']} jobs: {jobs_named}"
                         + "".join(f"; {b}" for b in back))
    for name, p in sorted(plat.items()):
        if not p["misses"] or p["reported"]:
            continue
        secs = now - when((p["real"] or {}).get("at") or p["since"])
        if p["misses"] >= STALE_RUNS or secs >= STALE_S:
            last = (p["real"] or {}).get("sha", "")[:10] or "tier 2 began (no real verdict yet)"
            lines.append(f"TIER2 STALE {name} unverified since {last}: {p['misses']} run(s) in a row without a "
                         f"runner, {secs / 3600:.0f} h without a real verdict")
            p["reported"] = True
    return lines, [str(r["id"]) for r in new], plat


def triage(repo, run_id):
    run = get(f"https://api.github.com/repos/{repo}/actions/runs/{run_id}")
    jobs = get(paged(run["jobs_url"])).get("jobs", [])
    v, names = judge(run, jobs) if run.get("conclusion") in JUDGED else (None, [])
    what = {"red": f"red: {', '.join(names) or '(none named)'}", "green": "green"}.get(v) or (
        f"no verdict: not run: {not_run(names)}" if names else "no verdict")
    out = [f"run {run['id']} {run['event']} {run['head_sha'][:10]} {run['conclusion']}, {what}: {run['html_url']}"]
    for j in jobs:
        if j.get("conclusion") in ("success", "skipped", "neutral"):
            continue
        steps = [s["name"] for s in j.get("steps", []) if s.get("conclusion") not in ("success", "skipped", None)]
        place = "" if started(j) else "; never given a runner"
        out.append(f"  job {j['name']}: {j.get('conclusion')}{place}; failed steps: {', '.join(steps) or '-'}")
        for a in get(paged(j["check_run_url"] + "/annotations")):
            msg = (a.get("message") or "").strip()[:4000].replace(chr(10), chr(10) + " " * 6)
            out.append(f"    {a.get('annotation_level')}: {a.get('title') or ''}: {msg}")
    return out


def listing(repo):
    out = []
    for r in runs(repo)[:5]:
        v, names = verdict(r)
        line = f"{r['created_at']} run {r['id']} {r['event']:<8} {r['head_sha'][:10]} {r['conclusion']}: "
        if v == "red":
            line += f"red: {', '.join(names) or '(none named)'}  {r['html_url']}"
        else:
            line += v or ("no verdict, not run: " + not_run(names) if names else "no verdict")
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
            now = when(os.environ["PE_CI_NOW"]) if os.environ.get("PE_CI_NOW") else time.time()
            lines, ids, plat = bounded(LIMIT_S, news, repo, value, now)
            for line in lines:
                print(line)
            with open(value + ".platforms.json.tmp", "w", encoding="utf-8") as f:
                json.dump(plat, f, indent=1, sort_keys=True)
            os.replace(value + ".platforms.json.tmp", value + ".platforms.json")
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
