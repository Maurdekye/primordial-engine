R18 below ~20 kbit/s: where pe-session's bytes go, against the spike (docs/p2-3-r18-bytes.md).
pe-<k>.log / spike-<k>.log: capped --kbps <k> [--end 30 at 16] --by-kind (pe-session harness / spike).
pe-whatif<w>-<k>.log: the same on pe-session with --what-if <w> (measurement only: the shaper and the
counters charge fewer bytes, as if the wire omitted those fields). Bits: 1 prev id, 2 ack head ids,
4 signature, 8 the signature of all but each author's last entry in a batch. 12 kbit/s baselines are
the committed capped-12kbps.log in r18-pe/ and r18/.
