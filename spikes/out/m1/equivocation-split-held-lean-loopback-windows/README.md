# The held split equivocation on the lean wire (pe-app on the core's defaults)

Measured on p2/entry-savings 4f7cbcc (pe-app's lean wire, @item:primordial-engine/pe-app-lean-wire-accept-msg-lean-key-versions-by),
Windows loopback, release, once: `bash crates/pe-app/m1-equivocate.sh <pe-app> <out> 7481 split-held`. The setup is
../equivocation-split-held-loopback-windows/README.md's, on the lean, signed-once wire instead of the explicit one.

- ALL EQUAL (`compare.txt`): members 1, 2 and 4 equal each other and their offline replays; member 3 removed. Every
  honest member counts version A. Versions are numbered in the order each member saw them, as in the explicit run.
- The outcome is the explicit run's; the timings are not (@shader-reviewer, review of f87f58b, rerun on its own
  build). Member 4 gets the successor naming A in lean form at 8.125 s. Rebuilt against its own version B it fails
  to verify, so pe-session fetches the explicit form, one round trip over the 120 ms-delayed link: the proof lands at
  8.248 s, not 8.126 s. The split lasts about 15 frames instead of 8.
- Rollback depths in this run (`key-lines.txt`):
  - members 1 and 2: 16 at the proof (8.254 s);
  - member 4: 15, 16 and 17 between the proof and the removal decision (8.56 s, removed [3]), then 35 when the
    decision replaces its version B with A.
  - @shader-reviewer's rerun gave 16 at members 1-2 and 15 / 20 at member 4, against 8 / 9 and 8 / 16 on the
    explicit wire.
- `key-lines.txt`: the split, member 3's touch seq 234 as each member saw it, the FINAL lines and the rollbacks.
