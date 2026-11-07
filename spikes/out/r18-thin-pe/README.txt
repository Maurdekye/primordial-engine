The spike's thin-link cells (scripts/run_r18_thin.sh) run on pe-session with thin links Part A (--pe), batched +
adaptive only. docs/thin-links-part-a-pe.md has the comparison with the spike's spikes/out/r18-thin and the measured
gaps.

pe-main-b58fe5c/: 18 of the cells on pe-session before the port (main b58fe5c), the ones where the port is worse
than the spike, for "the port against pe main". That harness has no --adaptive-only and no --gestures, so these logs
hold all three transports; compare their batched+adaptive lines.
