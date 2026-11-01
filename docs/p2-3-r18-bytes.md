# pe-session's R18 below ~20 kbit/s: where the bytes go

Item: pe-session's R18 below ~20 kbit/s against the spike (@world-chunks, 2026-11-01). Tree: main ebec136
(7.6 ack reductions and D56's idle gate in). Logs: `spikes/out/r18-pe/bytes/` (README there).

## Finding: the gap is per-entry size, not entry count [V]

`capped --by-kind` (both harnesses) splits each member's offered bytes during the gesture by message
kind, inside batches too. Batched + adaptive, member 1 (the author):

| Cap | | Hand entries | Acks | Datagram overhead | Total kbit/s |
|---|---|---|---|---|---|
| 256 (no queue) | pe-session | 60.1/s x 133 B | 30.1/s x 170 B | 60.1/s x 52 B | 131.2 |
| | spike | 60.1/s x 45 B | 30.1/s x 72 B | 60.1/s x 52 B | 65.2 |
| 32 | pe-session | 14.2/s x 171 B | 10.9/s x 170 B | 22.1/s | 43.7 |
| | spike | 24.0/s x 62 B | 15.0/s x 72 B | 30.1/s | 33.6 |
| 16 | pe-session | 13.7/s x 173 B | 10.4/s x 169 B | 22.4/s | 42.6 |
| | spike | 18.9/s x 70 B | 13.1/s x 71 B | 26.0/s | 29.2 |

Uncapped, the two send the same messages at the same rates. Only the sizes differ. From the two wire
formats (`pe-session/src/session/entry.rs`, `spikes/sim-core/src/session.rs`):

- A pe-session entry carries an Ed25519 signature (64 B) that the spike's does not.
- Its prev link is a 32-byte EntryId; the spike's is 8 B (+24).
- Each ack head names a 32-byte id; the spike's is 8 B (+24 per head).
- Its check hash is 32 B against the spike's 8, but reduction R1 sends it only when the check frame
  changes, so on average it costs less than the spike's.

So a hand is +88 B (64 + 24) and an ack +98 B (64 + 24 + 24, less R1's saving). The controller sees
congestion and thins the same way in both. Every entry it still sends is about 2.4x the spike's.

## What each saving would buy [V in the simulation; the codecs are not built]

`capped --what-if <bits>` (pe-session harness, measurement only): the bandwidth shaper and the counters
charge each message fewer bytes, as if the wire left those fields out. Everything else is unchanged:
same protocol, same entries, same timing. Adaptive transport, single gesture:

| What the wire omits | 32 kbit/s: queue, stalled, back | 16 kbit/s: stalled, back | 12 kbit/s: stalled, back |
|---|---|---|---|
| nothing (pe-session today) | 493 ms, 168, 1.9 s | 1,535, never | 1,538, never |
| prev id and ack head ids (3) | 399 ms, 0, at once | 936, 14.9 s | 1,530, never |
| every signature (4) | 313 ms, 0, at once | 770, 12.1 s | 1,525, never |
| ids, and every signature but each author's last in a datagram (11) | 401 ms, 0, at once | 474, 7.3 s | 1,470, never |
| ids and every signature (7) | 120 ms, 0, at once | 176, 2.4 s | 366, 6.1 s |
| the spike, for reference | 213 ms, 0, at once | 449, 7.3 s | 874, 13.6 s |

- Signing only the last own entry per datagram (8) saves almost nothing by itself: at these levels few
  datagrams carry two of the author's entries. With implicit ids (11) it matches the spike at 16
  kbit/s.
- The end states are EQUAL in every what-if run except those that stay collapsed at 12 kbit/s (3, 4,
  11). There the compared frame is the stalled tip, as in today's 12 kbit/s log.

## Options (for a ruling; none is built)

1. **Implicit ids on the wire** (3). The receiver rebuilds `prev` from the author's entry at `seq - 1`
   and each ack head's id from its own entry at `(q, s)`. The signature still covers the full ids, so a
   mismatch (a fork, or an entry the receiver lacks) fails verification. The receiver then fetches the
   explicit form, so the equivocation evidence is unchanged. Codec-local, plus a hold or fetch path for
   entries whose predecessor is missing [I]. Fixes 32 kbit/s; at 16 kbit/s it recovers, but in 15 s.
2. **Option 1 plus one signature per datagram per author** (11). An author's entries are hash-chained,
   so a signature on the last one covers the earlier ones in the datagram. A relayed or evidence entry
   travels with its signed successor [I]. Matches the spike at 16 kbit/s; still collapses at 12.
3. **Link MACs for live traffic, signatures amortized** (towards 7). Each datagram carries a pairwise
   MAC (16 B, keys from the members' X25519 exchange); every Kth own entry, or each check frame, is
   signed, and the chain covers the rest. Third parties verify through the signed successor [I].
   What-if 7 recovers at 12 kbit/s too, faster than the spike, but it charges neither the MAC nor the
   periodic signatures, so it is a bound, not a measurement of this option. Changes the security design (S3: every entry is
   signed), so it needs @engine-architect and a user ruling.

Not an M1 gate: M1 runs on a LAN (coordinator, 2026-11-01).
