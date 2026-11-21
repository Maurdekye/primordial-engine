r"""The self-test's step agent (the pull-ahead plan's T2; @api-eng). An MCP client that does M1's steps 5 and 6 the way
the user's agent did in the redo (2026-11-20), checks each through world_query, and writes what T4's checker reads.

Run by pe-app as its agent; T3 (selftest.py) passes it as `--agent "python -I scripts/selftest_agent.py ..."`:

  pe-app ... --mcp-http --agent-name step --agent python -I selftest_agent.py [--steps 5,6,limit] [options]

It reads PE_MCP_URL and PE_MCP_TOKEN, as pe-app gives them to its agent, and waits for `--members` players.
  5      step 5's four edits: a red cube on the pit's floor (from a query's output: `"mesh": null` included), the
         balls blue (one `set` over a selector), the cube doubled, the cube removed
  6      step 6: the lamp's blink script committed (`wait: final`; `unavailable` while a player joins is retried),
         attached once active, its Light replaced (a plain `add` of the Light the Lamp has must be refused first:
         F6) and its Blink added; then Light.intensity sampled every `--sample-every` s for `--sample-s` s
  limit  the world filled to its shape limit (520) by one bulk `duplicate`, then one more shape, which must be
         refused for the limit
Steps run in the order given. `--hold-s` keeps sampling the lamp, if step 6 ran, until pe-app ends the agent or the
time runs out, so T3's timeline (a fault, the repair, the leave) happens while the lamp is watched.

Out (`--out`, else the run folder T3 made, found from PE_CUT_LOG's folder's parent, plus `agent`):
  lamp.jsonl   {"t": <Unix seconds>, "intensity": <float>}, one a sample (T4's lamp and lamp_through_split)
  limit.jsonl  {"shapes": <n>, "ok": <bool>, "error": <text or null>} for 520 and 521 (T4's limit)
  steps.jsonl  every call and check: {"t", "step", "what", "ok", "code", "message", "ms"}
Exit 0 when every check passed, 1 when one failed, 2 when it can't start. Standard library only.
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

SHAPE_LIMIT = 520
ROOT = "/Ball pit"
LAMP = "/Ball pit/Lamp"

# Step 6's script as the user's agent wrote it in the redo (claude-code-transcript.jsonl, row 374).
BLINK = """#![no_std]
#![forbid(unsafe_code)]
use pe_sdk::engine::Light;

pe_sdk::component! {
    /// How the lamp blinks: a full on/off cycle every `period` seconds, shifted by `phase`.
    pub struct Blink { period: f32, phase: f32 }
}

pe_sdk::system! {
    /// Switches the lamp's light on and off on its period.
    fn blink(stage: PrePhysics, read: [blink: Blink], write: [light: Light]) |ctx, me| {
        let t = ctx.time_s() + me.blink.phase;
        let on = t % me.blink.period < me.blink.period * 0.5;
        me.light.intensity = if on { 1.0 } else { 0.0 };
    }
}

pe_sdk::script!(components: [Blink], systems: [blink]);
"""

# Step 5's cube as the redo's agent created it (row 110): a query's output given back, `"mesh": null` and all.
CUBE = {
    "Transform": {"position": {"offset": [0, -0.2, 0], "relative_to": "parent"}, "rotation_deg": [0, 0, 0],
                  "scale": [1, 1, 1]},
    "Shape": {"kind": 1, "density": 20, "size": [0.1, 0.1, 0.1], "mesh": None},
    "Body": {"kind": 2, "friction": 0.6, "restitution": 0, "linear_damping": 0.1, "angular_damping": 0.3,
             "rolling_resistance": 0.2, "gravity_scale": 1, "bullet": False, "may_sleep": True, "grabbable": True},
    "Appearance": {"base": {"linear": [0.85, 0.15, 0.15, 1]}, "emissive": "#000000", "material": None,
                   "metalness": 0, "roughness": 0.6},
}
BLUE = [0.1, 0.3, 0.95, 1]
LIGHT = {"kind": 0, "intensity": 1, "range": 3, "color": {"linear": [1, 0.8, 0.6]}, "inner_angle": 0,
         "outer_angle": 0}


class Mcp:
    """MCP over pe-app's streamable HTTP, stateless JSON (as stirrer.py)."""

    def __init__(self, url, token):
        self.url, self.token, self.n = url, token, 0

    def call(self, method, params=None, timeout=90):
        self.n += 1
        body = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None:
            body["params"] = params
        req = urllib.request.Request(
            self.url, data=json.dumps(body).encode(), method="POST",
            headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json",
                     "Accept": "application/json, text/event-stream"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            text = r.read().decode("utf-8")
        if text.lstrip().startswith("data:") or "\ndata:" in text:
            text = "\n".join(l[5:].strip() for l in text.splitlines() if l.startswith("data:"))
        return json.loads(text)

    def tool(self, name, args):
        """(ok, result): the structured content, or the error envelope `{code, message, ...}` when not ok."""
        r = self.call("tools/call", {"name": name, "arguments": args})
        if "error" in r:
            return False, {"code": "rpc", "message": str(r["error"])}
        res = r.get("result", {})
        out = res.get("structuredContent")
        if out is None:
            texts = [c.get("text", "") for c in res.get("content", []) if c.get("type") == "text"]
            try:
                out = json.loads(texts[0]) if texts else {}
            except ValueError:
                out = {"message": texts[0] if texts else ""}
        return not res.get("isError"), out


class Agent:
    def __init__(self, mcp, out):
        self.m, self.out, self.failed = mcp, out, []
        os.makedirs(out, exist_ok=True)
        self.files = {k: open(os.path.join(out, k + ".jsonl"), "a", encoding="utf-8")
                      for k in ("steps", "lamp", "limit")}
        self.step = "start"

    def write(self, kind, row):
        f = self.files[kind]
        f.write(json.dumps(row) + "\n")
        f.flush()

    def say(self, *a):
        print("step agent:", *a, flush=True)

    def tool(self, what, name, args):
        t0 = time.time()
        ok, r = self.m.tool(name, args)
        self.write("steps", {"t": round(t0, 3), "step": self.step, "what": what, "ok": ok,
                             "code": None if ok else r.get("code"), "message": None if ok else r.get("message"),
                             "details": None if ok else r.get("details"), "ms": round((time.time() - t0) * 1e3)})
        return ok, r

    def check(self, what, cond, detail=""):
        self.write("steps", {"t": round(time.time(), 3), "step": self.step, "what": "check: " + what, "ok": bool(cond),
                             "code": None, "message": detail or None, "ms": 0})
        if not cond:
            self.failed.append(f"step {self.step}: {what} {detail}".strip())
            self.say("FAILED:", what, detail)
        return bool(cond)

    def apply(self, what, ops, wait="local", label=None, retry_unavailable_s=0):
        args = {"label": label or what, "ops": ops, "wait": wait}
        deadline = time.time() + retry_unavailable_s
        while True:
            ok, r = self.tool(what, "world_apply", args)
            joining = r.get("code") == "unavailable" and "joining" in str(r.get("message"))
            if ok or not joining or time.time() >= deadline:
                return ok, r
            self.say(what, "unavailable, retrying:", r.get("message"))
            time.sleep(2)

    def query(self, what, select, fields=None, **more):
        q = {"where": select}
        if fields:
            q["fields"] = fields
        q.update(more)
        ok, r = self.tool(what, "world_query", q)
        return r if ok else None

    def one(self, what, path, fields=None):
        r = self.query(what, {"path": path}, fields)
        objs = (r or {}).get("objects") or []
        return objs[0] if objs else None

    # ---- the session ----------------------------------------------------------------------------------------------

    def wait_for(self, members, timeout_s):
        self.step = "join"
        t0 = time.time()
        while time.time() - t0 < timeout_s:
            ok, s = self.tool("session_info", "session_info", {})
            n = len(s.get("members", [])) if ok else 0
            if n >= members and s.get("caught_up", True):
                self.say(f"{n} member(s), caught up")
                return True
            time.sleep(2)
        return self.check(f"{members} members within {timeout_s} s", False)

    # ---- step 5 ---------------------------------------------------------------------------------------------------

    def step5(self):
        self.step = "5"
        root = self.one("the pit", ROOT)
        if not self.check("the pit is found by path", root and root.get("name") == "Ball pit", ROOT):
            return
        # 5a. "put a red cube on the floor of the pit"
        ok, r = self.apply("a red cube", [{"op": "create", "parent": root["id"], "name": "Red cube", "as": "$cube",
                                           "components": CUBE}])
        cube = (r.get("created") or {}).get("$cube") if ok else None
        if not self.check("the cube is created", cube, "" if ok else f"{r.get('code')}: {r.get('message')}"):
            return
        seen = self.one("the cube", ROOT + "/Red cube", ["Shape.size", "Appearance.base"])
        self.check("the cube is in the world", seen and seen["id"] == cube["id"])
        # 5b. "make the balls blue": one op over a selector
        balls = {"all": [{"under": root["id"]}, {"name_like": "Ball"}]}
        n = (self.query("count the balls", balls, count_only=True) or {}).get("count") or 0
        ok, r = self.apply("the balls blue", [{"op": "set", "where": balls, "field": "Appearance.base",
                                               "value": {"linear": BLUE}, "if_version": "any", "max": 1000}])
        self.check("the balls are made blue", ok and r.get("count") == n, f"count {r.get('count')} of {n} balls")
        page = self.query("a page of balls", balls, ["Appearance.base"], limit=20) or {}
        colours = [o.get("components", {}).get("Appearance", {}).get("base") for o in page.get("objects", [])]
        blue = all(isinstance(c, dict) and [round(x, 2) for x in c.get("linear", [])] == BLUE for c in colours)
        self.check("the balls read back blue", colours and blue, json.dumps(colours[:2]))
        # 5c. "double the cube"
        ok, r = self.apply("the cube doubled", [{"op": "set", "id": cube["id"], "field": "Shape.size",
                                                 "value": [0.2, 0.2, 0.2], "if_version": cube["version"]}])
        self.check("the cube is doubled", ok, "" if ok else f"{r.get('code')}: {r.get('message')}")
        seen = self.one("the doubled cube", ROOT + "/Red cube", ["Shape.size"])
        size = (seen or {}).get("components", {}).get("Shape", {}).get("size")
        self.check("the cube reads back doubled", size and [round(x, 3) for x in size] == [0.2, 0.2, 0.2], str(size))
        # 5d. "remove the cubes"
        ok, r = self.apply("the cube removed", [{"op": "delete", "id": cube["id"]}])
        self.check("the cube is removed", ok, "" if ok else f"{r.get('code')}: {r.get('message')}")
        gone = self.query("the cube, gone", {"path": ROOT + "/Red cube"}, count_only=True) or {}
        self.check("the cube is gone", gone.get("count") == 0, str(gone.get("count")))

    # ---- step 6 ---------------------------------------------------------------------------------------------------

    def step6(self, join_wait_s):
        self.step = "6"
        lamp = self.one("the lamp", LAMP, ["Light.intensity"])
        if not self.check("the lamp is found by path", lamp and lamp.get("name") == "Lamp", LAMP):
            return False
        ok, r = self.apply("the blink script", [{"op": "code", "id": "$blink", "source": BLINK}], wait="final",
                           label="Commit lamp blink script", retry_unavailable_s=join_wait_s)
        script = ((r.get("scripts") or {}).get("$blink") or {}).get("id") if ok else None
        if not self.check("the script is committed", script, "" if ok else f"{r.get('code')}: {r.get('message')}"):
            return False
        attach = [{"op": "attach", "script": script, "to": [lamp["id"]], "mode": "linked"}]
        # Active: an attach's dry run is accepted (before, it is refused `not_ready`).
        t0 = time.time()
        while True:
            ok, r = self.tool("attach, dry run", "world_apply", {"ops": attach, "mode": "dry_run"})
            if ok or r.get("code") != "not_ready" or time.time() - t0 > 120:
                break
            time.sleep(1)
        if not self.check("the script activates", ok, "" if ok else f"{r.get('code')}: {r.get('message')}"):
            return False
        ok, r = self.apply("the script attached", attach)
        self.check("the script is attached", ok, "" if ok else f"{r.get('code')}: {r.get('message')}")
        # F6: the Lamp has a Light, so a plain add is refused, showing what it has; `replace: true` replaces it.
        ok, r = self.apply("a plain add of the Light", [{"op": "add", "id": lamp["id"], "component": "Light",
                                                         "fields": LIGHT}])
        self.check("a plain add of a Light the Lamp has is refused invalid (F6)",
                   not ok and r.get("code") == "invalid", f"{r.get('code')}: {r.get('message')}")
        ok, r = self.apply("the Light replaced", [{"op": "add", "id": lamp["id"], "component": "Light",
                                                   "fields": LIGHT, "replace": True}])
        self.check("the Light is replaced", ok, "" if ok else f"{r.get('code')}: {r.get('message')}")
        ok, r = self.apply("the Blink added", [{"op": "add", "id": lamp["id"], "component": "Blink",
                                                "fields": {"period": 1, "phase": 0}}])
        self.check("the Blink is added", ok, "" if ok else f"{r.get('code')}: {r.get('message')}")
        self.lamp = lamp["id"]
        return True

    def sample(self, seconds, every):
        """Light.intensity on the lamp, every `every` s for `seconds` s: one lamp.jsonl row each."""
        self.step = "sample"
        end, levels, flips, last, n = time.time() + seconds, set(), 0, None, 0
        while time.time() < end:
            t = time.time()
            try:
                ok, r = self.m.tool("world_query", {"where": {"id": self.lamp}, "fields": ["Light.intensity"]})
            except (urllib.error.URLError, OSError, ValueError):
                self.say("the session ended while sampling")
                break
            objs = r.get("objects") or [] if ok else []
            x = objs[0].get("components", {}).get("Light", {}).get("intensity") if objs else None
            if isinstance(x, (int, float)):
                self.write("lamp", {"t": round(t, 3), "intensity": x})
                levels.add(x)
                flips += last is not None and x != last
                last, n = x, n + 1
            time.sleep(max(0.0, every - (time.time() - t)))
        return n, levels, flips

    # ---- the shape limit ------------------------------------------------------------------------------------------

    def limit(self):
        self.step = "limit"
        shapes = lambda: ((self.query("count the shapes", {"has": "Shape"}, count_only=True) or {}).get("count"))
        n = shapes()
        ball = self.one("a ball to copy", ROOT + "/Ball 0")
        if not self.check("the shape count and a ball", isinstance(n, int) and ball, f"{n} shapes"):
            return
        if n < SHAPE_LIMIT:
            k = SHAPE_LIMIT - n
            ok, r = self.apply(f"{k} copies to the limit", [{"op": "duplicate", "id": ball["id"], "count": k,
                                                             "step": [0.0, 0.13, 0.0]}])
            self.write("limit", {"shapes": SHAPE_LIMIT, "ok": ok, "error": None if ok else r.get("message")})
            self.check(f"{SHAPE_LIMIT} shapes accepted", ok, "" if ok else f"{r.get('code')}: {r.get('message')}")
        elif n == SHAPE_LIMIT:
            self.write("limit", {"shapes": SHAPE_LIMIT, "ok": True, "error": None})
        else:
            return self.check("at most the limit to start", False, f"{n} shapes")
        self.check("the world holds the limit", shapes() == SHAPE_LIMIT)
        ok, r = self.apply("one past the limit", [{"op": "duplicate", "id": ball["id"]}])
        self.write("limit", {"shapes": SHAPE_LIMIT + 1, "ok": ok, "error": None if ok else r.get("message")})
        self.check(f"shape {SHAPE_LIMIT + 1} refused too_large", not ok and r.get("code") == "too_large",
                   f"{r.get('code')}: {r.get('message')}")


def out_dir(given):
    if given:
        return given
    cuts = os.environ.get("PE_CUT_LOG")
    if cuts:
        return os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(cuts))), "agent")
    return os.path.abspath("agent")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--steps", default="5,6", help="comma-separated: 5, 6, limit, in order")
    ap.add_argument("--members", type=int, default=2)
    ap.add_argument("--join-timeout-s", type=float, default=600)
    ap.add_argument("--settle-s", type=float, default=5, help="after the players are in, before step 5")
    ap.add_argument("--sample-s", type=float, default=20)
    ap.add_argument("--sample-every", type=float, default=0.1)
    ap.add_argument("--hold-s", type=float, default=0, help="keep sampling the lamp after the steps, up to this")
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    url, token = os.environ.get("PE_MCP_URL"), os.environ.get("PE_MCP_TOKEN")
    if not url or not token:
        print("step agent: PE_MCP_URL and PE_MCP_TOKEN are needed (run it as pe-app's --agent)", flush=True)
        return 2
    m = Mcp(url, token)
    m.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                          "clientInfo": {"name": "pe-step-agent", "version": "0.1"}})
    ag = Agent(m, out_dir(a.out))
    ag.say("writing to", ag.out)
    if not ag.wait_for(a.members, a.join_timeout_s):
        return 1
    time.sleep(a.settle_s)
    lamp_on = False
    for s in [x.strip() for x in a.steps.split(",") if x.strip()]:
        if s == "5":
            ag.step5()
        elif s == "6":
            lamp_on = ag.step6(a.join_timeout_s)
            if lamp_on:
                n, levels, flips = ag.sample(a.sample_s, a.sample_every)
                ag.check("the lamp blinks", len(levels) >= 2 and flips >= 4,
                         f"{n} samples, levels {sorted(levels)[:4]}, {flips} changes")
        elif s == "limit":
            ag.limit()
        else:
            ag.check(f"a known step ({s})", False)
    if lamp_on and a.hold_s > 0:
        ag.sample(a.hold_s, a.sample_every)
    ag.step = "end"
    ag.say("PASS" if not ag.failed else f"FAIL: {len(ag.failed)} check(s): " + "; ".join(ag.failed))
    return 0 if not ag.failed else 1


if __name__ == "__main__":
    sys.exit(main())
