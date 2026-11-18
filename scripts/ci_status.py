"""Tier 2's state (docs/refound-plan.md 4.6, decision C7): the hosted runs of .github/workflows/gate.yml on the
public repository's main. docs/landing-gates.md describes tier 2.

usage: python -I scripts/ci_status.py [--repo OWNER/NAME] [--token-file FILE]
                                      [--check FILE [--cache FILE] | --new STATEFILE | --run ID]
  (no option)        the latest finished runs on main, each with its verdict.
  --check FILE       the landing gate's C7 step (scripts/gate.sh, step 5). Exit 1 if tier 2 is red: the latest
                     finished run on main that gives a verdict is red, and its id isn't listed in FILE
                     (scripts/tier2_explained.txt as the landing has it, `<run id> -- <reason>`). Exit 0 if that run
                     is green or explained, or there is none yet; 3 if tier 2's state is unknown: GitHub can't be
                     read, or none of the 5 newest runs it judges gives a verdict (it reads no further: chunks-
                     reviewer's L1; a stretch of runs a platform missed would cost a call each).
  --cache FILE       with --check: keep each finished run's verdict in FILE, under the run's id and attempt, so a
                     later check reads only the listing for a run it has judged. A finished attempt never changes,
                     and a re-run is a new attempt, judged afresh. The gate keeps FILE in the clone's git folder,
                     shared by its worktrees (coordinator). A FILE that can't be read or written only
                     costs calls: it never changes a verdict or the exit code.
  --new STATEFILE    for the triage watchdog, nothing when all is well, otherwise:
                     `TIER2 RED <run id> <conclusion> <url> jobs: <red jobs>`, once per red run, followed by
                     `; <platform> unverified since <sha>, up to <sha>` for each platform that is back after runs
                     without a runner; and `TIER2 STALE <platform> unverified since <sha>: ...`, once per stretch,
                     when a platform has had no real verdict for 24 h or for 3 runs in a row. STATEFILE keeps the
                     run ids seen; STATEFILE.platforms.json keeps each platform's last real verdict and its misses.
  --run ID           for triage: that run's verdict, the jobs that didn't pass, their failed steps and their
                     annotations. The workflow writes the failing tests, their panic messages and any differing
                     digest there, because annotations are public and logs are not.
  --token-file FILE  read GitHub with the token FILE holds (the gate passes git config pe.gate.ghtoken), so the
                     calls count against its 5,000 an hour, not this address's 60 (the user's ruling, coordinator: a read-only token, kept outside any repository). The token goes only to
                     https://api.github.com, in a header a redirect doesn't carry on, and is never printed. A missing
                     or empty FILE means no token; so does one holding anything else, which the output notes. If
                     GitHub refuses the token (HTTP 401 or 403), the call is made again without it, the rest of the
                     run reads without it, and the output says so.
Only main's own runs count: a push, the nightly schedule or a manual dispatch, in the repository itself. A pull
request's run never does, not even one whose branch is a fork's main: GitHub's branch filter matches a run's head
branch, whichever repository it comes from (chunks-reviewer's M1).
A finished run's jobs give its verdict (coordinator). It is red when a job failed or timed out, or
started and was then cancelled: GitHub reports a job stopped at its time limit as cancelled, like one cancelled by
hand. Failing that, a job GitHub never gave a runner leaves the run with no verdict: that platform is "not run"
(GitHub had no capacity for it: "not acquired"), and its result is unknown, neither red nor green. So does a run
cancelled before any job started (a burst of pushes cancels the runs still waiting). Failing both, the run's own
conclusion decides: success or neutral is green; failure, timed_out or startup_failure is red; anything else gives
no verdict. The check looks past a run with no verdict to the run before.
It reads GitHub's public API, without a token unless --token-file gives one (60 requests an hour per address; an
answer of 304 counts too, measured 2026-11-17): the listing, then each judged run's jobs (two calls in all,
usually, and one with --cache once the run is judged; --new judges at most 5 new runs a call). --check, --new and
the listing give up after 20 s in all, whatever holds them up (a DNS lookup, a server that trickles bytes),
because the landing hook runs --check while git holds main's locks; --run gives up after 60 s. A 404 on the
listing means the workflow has no run yet.
PE_CI_RUNS_URL replaces the listing's URL (the self-test points it at a fixture, or at a server that never
finishes its answer), and PE_CI_NOW the time --new measures 24 h from.
Exit 2 on a usage error, 4 if this script fails in any other way, so that a crash never reads as a red run.
"""

import json
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.parse
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
CHECK_AT_MOST = 5  # runs --check judges (a call each, unless --cache has it) before it calls tier 2's state unknown
API_HOST = "api.github.com"  # the one host --token-file's token is sent to, over https
# A token's characters (github_pat_..., ghp_...). Nothing else is ever sent: a header value with a line break or
# a space could reach an error message (http.client quotes a bad value), and the token must never be printed.
TOKEN = re.compile(r"[A-Za-z0-9_]{20,255}")
auth = {"token": None, "notes": []}  # --token-file's token, and what the output says about it (never the token)


class Unreadable(Exception):
    pass


def load_token(path):
    """The token in the file at `path`, or None. A missing, unreadable or empty file is no token; so is a file that
    holds anything but one token, which is noted without its text. A byte-order mark, blanks and a final line break
    around it are allowed: Notepad and PowerShell write them."""
    try:
        with open(path, "rb") as f:
            raw = f.read(4097)
    except OSError:
        return None
    text = None  # a file over 4 KB is no token
    if len(raw) <= 4096:
        codec = "utf-8"
        for bom, name in ((b"\xef\xbb\xbf", "utf-8"), (b"\xff\xfe", "utf-16-le"), (b"\xfe\xff", "utf-16-be")):
            if raw.startswith(bom):
                raw, codec = raw[len(bom):], name
                break
        try:
            text = raw.decode(codec).strip()
        except UnicodeDecodeError:
            pass
    if text == "":
        return None
    if text is None or not TOKEN.fullmatch(text):
        auth["notes"].append("the token file holds no token, so GitHub was read without one")
        return None
    return text


def token_for(url):
    """The token for a request to `url`, or None: it goes to GitHub's API over https on its own port, and to no
    other address (a fixture, a test server, a URL with a user name in it)."""
    if not auth["token"]:
        return None
    try:
        u = urllib.parse.urlsplit(url)
        port = u.port
    except ValueError:
        return None
    if u.scheme == "https" and u.hostname == API_HOST and port in (None, 443) and "@" not in u.netloc:
        return auth["token"]
    return None


def request(url):
    """The request for `url`. The token rides in an unredirected header: urllib copies a request's other headers to
    a redirect's target, which may be another host."""
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json",
                                               "User-Agent": "primordial-engine-ci-status"})
    token = token_for(url)
    if token:
        req.add_unredirected_header("Authorization", f"Bearer {token}")
    return req


def get(url, missing_ok=False):
    req = request(url)
    try:
        with urllib.request.urlopen(req, timeout=LIMIT_S) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        if e.code in (401, 403) and req.has_header("Authorization"):
            # Refused: revoked, expired or not allowed. The rest of this run reads as it would without the file.
            auth["token"] = None
            auth["notes"].append(f"GitHub refused the token (HTTP {e.code}), so GitHub was read without it")
            return get(url, missing_ok)
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


def run_key(run):
    """A finished run's verdict never changes within an attempt; re-running it makes a new attempt."""
    return f"{run['id']}:{run.get('run_attempt') or 1}"


def load_cache(path):
    try:
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def save_cache(path, cache, loaded, keys):
    """Keeps the entries of the runs just listed (`keys`), and writes only a change. Another check may write at the
    same moment: each writes whole files through its own temporary one, and a lost entry costs one call later."""
    kept = {k: v for k, v in cache.items() if k in keys}
    if kept == loaded:
        return
    tmp = f"{path}.{os.getpid()}.tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(kept, f, indent=0, sort_keys=True)
        os.replace(tmp, path)
    except OSError:
        try:
            os.remove(tmp)
        except OSError:
            pass


def cached_verdict(run, cache):
    """verdict(run), through `cache` (a dict; None for no cache). An entry is used only in the shape this writes.
    A verdict is kept only when every job has its conclusion, so a jobs list GitHub hasn't finished is read again,
    and from no jobs only for a run cancelled before any job started or one that couldn't start: any other finished
    run has jobs, so an empty list is read again too (chunks-reviewer's nit)."""
    if cache is None or run.get("conclusion") not in JUDGED:
        return verdict(run)
    key = run_key(run)
    hit = cache.get(key)
    if (isinstance(hit, dict) and set(hit) == {"verdict", "names"} and hit["verdict"] in ("red", "green", None)
            and isinstance(hit["names"], list) and all(isinstance(n, str) for n in hit["names"])):
        return hit["verdict"], hit["names"]
    jobs = jobs_of(run)
    v, names = judge(run, jobs)
    if all(j.get("conclusion") for j in jobs) and (jobs or run.get("conclusion") in ("cancelled", "startup_failure")):
        cache[key] = {"verdict": v, "names": names}
    return v, names


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


def check(repo, path, cache_path=None):
    """(exit code, message) for the gate."""
    rs = runs(repo)
    if not cache_path:
        return judge_newest(rs, path, None)
    loaded = load_cache(cache_path)
    cache = dict(loaded)
    try:
        return judge_newest(rs, path, cache)
    finally:  # what was judged is kept, even when a later call fails
        save_cache(cache_path, cache, loaded, {run_key(r) for r in rs})


def judge_newest(rs, path, cache):
    gaps, judged = [], 0
    for n, r in enumerate(rs):
        if r.get("conclusion") in JUDGED:
            if judged == CHECK_AT_MOST:
                return 3, (f"no verdict in the {judged} newest runs judged"
                           f"{' (not run: ' + '; '.join(gaps) + ')' if gaps else ''}; older runs not read")
            judged += 1
        v, names = cached_verdict(r, cache)
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


def noted(line):
    """`line`, with what the output says about the token, if anything: the gate shows a check's last line."""
    return line + "".join(f"; {n}" for n in auth["notes"])


def main():
    args, repo, mode, value, cache = sys.argv[1:], os.environ.get("PE_CI_REPO", REPO), None, None, None
    token_file = None
    while args:
        a = args.pop(0)
        if a == "--repo" and args:
            repo = args.pop(0)
        elif a == "--cache" and args and cache is None:
            cache = args.pop(0)
        elif a == "--token-file" and args and token_file is None:
            token_file = args.pop(0)
        elif a in ("--check", "--new", "--run") and args and mode is None:
            mode, value = a, args.pop(0)
        else:
            print(__doc__)
            return 2
    if cache is not None and mode != "--check":
        print(__doc__)
        return 2
    if token_file:
        auth["token"] = load_token(token_file)
    try:
        if mode == "--check":
            code, msg = bounded(LIMIT_S, check, repo, value, cache)
            print(noted(f"ci_status: {msg}"))
            return code
        if mode == "--new":
            now = when(os.environ["PE_CI_NOW"]) if os.environ.get("PE_CI_NOW") else time.time()
            lines, ids, plat = bounded(LIMIT_S, news, repo, value, now)
            for line in lines + [f"ci_status: note: {n}" for n in auth["notes"]]:
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
        print("\n".join(lines + [f"ci_status: note: {n}" for n in auth["notes"]]))
        return 0
    except Unreadable as e:
        print(noted(f"ci_status: GitHub can't be read ({e})"))
        return 3


if __name__ == "__main__":
    try:
        rc = main()
    except BaseException as e:  # a crash must not exit 1, which means red
        print(f"ci_status: failed: {type(e).__name__}: {e}")
        rc = 4
    sys.stdout.flush()
    sys.exit(rc)
