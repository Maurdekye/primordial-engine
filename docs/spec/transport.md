# pe-net: the transport

Status: draft 1.6, 2026-11-11, by @shader-lang (WS-net). Built so far: pe-net-crypto (2, 8; landed). pe-net's
clock and lanes are in progress on branch `net/pe-net`. Draft 1.3 limits redundant rides to spare space on a link
with room (3.3, 4.2, 9, 10), found while building the lanes; 1.4 gates them on the delay guard as well as the
budget, and 1.5 fixes the cell that tests them (@design-reviewer's reviews of 1.3 and 1.4). Draft 1.6 records
what building the Core settled: the welcome exchange and its up record (6.4), the stream kinds (3.6), and the
Core's interface and network thread as built (7). Draft 1.2 records the user's ruling on the firewall default
(m4534df91cc48; 2.4, 9, 11); draft 1.1 answered @design-reviewer's review of draft 1.
- **Draft 0.1** (landed, main 25e1a29, approved by @design-reviewer) holds section 2: the rules M0's QUIC
  feasibility check settled, with the coordinator's rulings (mc64f7a24509e) and the review's additions.
- **Draft 1** adds sections 3-12, the rest of WS-net's M1 scope (`docs/refound-plan.md`, branch
  `docs/refound-plan`, the WS-net row): the S4 lanes, the rate budget v0, the clock, the connection lifecycle,
  threads, the harness, tests, parameters and open questions. It also folds @chunks-reviewer's notes on the M0
  report into 2.3 and 2.5. It follows the S4 shape agreed with @engine-architect, and seams v0 may revise it.
- **Evidence:** the M0 report, `spikes/quic-feasibility/REPORT.md` (branch `spike/quic-feasibility`,
  6ff52ec), and its logs in `spikes/out/quic-feasibility/`.
- **Time:** every limit is in seconds, never frames (the user's ruling md4b42d16d9df).

Marks: **[V]** verified, with what was checked named; **[S]** read in a crate's source; **[I]** inference or
design judgement.

## 1. What pe-net is for

- **Connections:** direct QUIC connections between peers, made from an invite, with no helper nodes assumed
  (user Q9). Over a LAN or the user's own VPN in M1.
- **Two kinds of traffic on one connection per peer pair:**
  - datagrams for live traffic: protocol messages and entries, repeated until acknowledged, and poses, newest
    only (3);
  - reliable streams, for repair, state and assets.
- **Also in M1:** the rate budget (v0); the clock, with gradual steering; and the version and self-test
  comparison when a connection opens.

## 2. Rules settled by M0

### 2.1 The stack: 100% Rust

- **QUIC:** `quinn-proto` 0.11 with no TLS feature. All of its rustls features pull `ring` or `aws-lc-rs` [S].
  Driven by pe-net's network thread; quinn-proto is a sans-IO state machine, so the harness can drive the same
  code [S, I].
- **TLS 1.3:** `rustls` 0.23 with no provider feature (`default-features = false, features = ["std"]`) [V].
- **Cryptography: our own small rustls provider** over the stable RustCrypto crates (coordinator ruling): one
  suite (TLS_AES_128_GCM_SHA256), one key exchange (X25519, `x25519-dalek`), one signature scheme (Ed25519,
  `ed25519-dalek`), with sha2, hmac and hkdf, and `aes-gcm` and `aes` for the QUIC packet and header keys.
  - Not `rustls-rustcrypto`: it was a pre-release (0.1.0-pre.0), and its QUIC module was unfinished [S].
  - M0 used it for key exchange and signatures only [V].
- **Our own glue between the layers:**
  - the QUIC keys (RFC 9001 5);
  - a port of quinn-proto's rustls adapter, with RustCrypto in place of ring for the retry tag, the
    stateless-reset HMAC and the token key (MIT/Apache-2.0 origin, noted in the file);
  - the raw-key verifiers (2.2).
- **quinn's convenience constructors** (`EndpointConfig::default()`, `ServerConfig::with_single_cert`, the
  endpoint helpers) are compiled only with ring [S]. pe-net builds each config explicitly, as the M0 spike
  does.
- **The audit:** the M0 spike passes `scripts/audit_native.py` on windows-x64, linux-x64, linux-arm64,
  macos-arm64 and android-arm64 [V for M0's stack, which used rustls-rustcrypto for key exchange and signatures;
  our own provider is not built yet, so it is unchecked]. The browser target (`wasm32-unknown-unknown`) does not
  pass, because quinn-proto depends on ring there [S], so a browser client is out of scope.
- **0-RTT is off:** no early data, no session resumption. Early data can be replayed, and nothing above the
  transport runs before the welcome anyway (2.3). A test checks that a 0-RTT attempt is refused [I].

### 2.2 Identity and invites

- **Keys:** each peer has an Ed25519 key. TLS carries it as a raw public key (RFC 7250): no certificate
  authority, name or expiry, and verifying the key is a byte comparison [V M0].
- **Both sides authenticate:**
  - the joiner pins the inviter's key from the invite;
  - the inviter receives the joiner's key in the handshake [V M0].
- **The invite:** carries the inviter's public key and the addresses it can be reached at; M0's form is
  `pe1:<key, hex>@<addr>,<addr>,...` [V].
  - The joiner tries the addresses and accepts only a handshake that proves the pinned key. A wrong key fails
    the handshake [V M0].
  - The production form adds a version, an expiry and a compact encoding. Whether it adds a one-time token is
    still open [I].
- **One key, two protocols:** the TLS raw key is the member's signing key, so the two uses must not overlap.
  - TLS 1.3 signs its CertificateVerify under its own context string.
  - Entry signatures (seam S1, WS-agree) need their own prefix, for example `"pe.entry"`, so that neither
    signature can stand in for the other.
  - Ed25519 verification uses `verify_strict` everywhere, since governance relies on refusing small-order keys
    [I, @design-reviewer's L2].

### 2.3 Admission is an explicit welcome

**Rule:** a joiner is admitted when it receives an application-level **welcome** message from the inviter,
never when the handshake completes.

- **Why:** in TLS 1.3 the client finishes its handshake before the server has checked the client's key.
- **Seen in M0** [V `win-loopback-allow-refused.log`]: the inviter refused a joiner's key, and the joiner
  still first reported "connected". It learned of the refusal only when the connection closed.

What follows from the rule:
- **The joiner:** sends nothing above the transport (no entries, no state requests) until the welcome
  arrives, and treats a close before the welcome as a refusal.
- **The inviter:** decides admission on the key the handshake proved: membership, bans, vouching, and an
  invite token if one is used. It then sends the welcome, or closes with a reason code.
- **When:** only after QUIC's handshake is *confirmed* (HANDSHAKE_DONE, RFC 9001 4.1.2), not merely completed.
  The welcome is the admission decision itself [@chunks-reviewer's addition].
- **The welcome carries [I]:** the version and self-test comparison of section 1, and the world's identity.
- **A welcome admits a connection, not a member.**
  - It is one peer accepting one connection.
  - Joining the session is pe-agree's join, decided at a cut.
  - Every later connection between the joiner and another member gets its own welcome, judged from the agreed
    governance (membership, bans), never from one peer's opinion. So M3's mesh and relays inherit no
    inviter-centred admission.

### 2.4 Reaching a peer behind a host firewall: simultaneous open

**Rule:** both peers send first.

- **The problem:** a host firewall that blocks unsolicited inbound UDP stops a joiner's first packet. Windows'
  firewall does this; seen in M0 [V]:
  - the active profile is BlockInbound, and the WSL Hyper-V firewall's default inbound action is Block;
  - a QUIC joiner, and a plain-UDP control, both got no answer.
- **The rule in practice:** the joiner's addresses reach the inviter, and each side sends a few packets to the
  other's addresses from its QUIC socket before and while the joiner dials. The firewall then sees the joiner's
  packets as replies.
- **How the joiner's addresses reach the inviter:**
  - **From a member already connected to both:** for every connection after the first. That member is the
    rendezvous.
  - **The first connection in M1, by default: Windows' own "allow access" prompt** (the user's ruling,
    m4534df91cc48: "(b) Allow prompt (rec.)").
    - The first time pe-app opens its socket on a Windows machine, Windows asks the user whether to allow it on
      their networks. The user's click creates the firewall rule, which is the user's go the coordinator's
      ruling asks for. pe-app creates no rule itself.
    - After that, the inviter accepts a joiner's dial directly.
    - pe-app says before the first listen what the prompt is for, and which networks to tick: private networks
      for a LAN, and whichever network the user's VPN shows as [I].
    - The prompt appears only in an interactive desktop session. Headless runs (CI, Session 0) and M0's test
      under WSL and Hyper-V never see it [I], so they use the fallback.
    - Declining the prompt usually leaves persistent *block* rules for the app, so the prompt doesn't return
      [I, @design-reviewer]. pe-app detects that state, uses the fallback, and tells the user how to undo the
      block if they want direct connections.
    - The rule is tied to the executable's path, so a build at another path prompts again. At the two-machine
      checkpoints, a new prompt after a new build is expected, not a fault.
  - **The fallback: a two-way invite** [I].
    - It is used when the prompt was declined or never shown, or the inviter isn't on Windows behind such a
      firewall.
    - The joiner dials the invite's addresses first.
    - If nothing answers within 3 s, it shows a short reply code: its key, its addresses, and the invite's id.
    - The user pastes the code into the inviter, and both sides then send at once, which needs no change to
      either machine.
  - M3's hole punching inherits the same channels.
- **Seen in M0** [V `win-server-linux-client-punched.log`]: three small packets from the inviter's socket to
  the joiner's address let the Linux joiner's handshake through to Windows.
- **Not done:** pe-app adding a firewall rule itself, at install or at run time. That would change the user's
  machine without their click (coordinator ruling).
- **Tests:** every test runs with no helper node (Q9). Hole punching through NATs and member relays are
  WS-net's M3 scope; this rule is their first step.

### 2.5 The security code is tested and reviewed on its own

Coordinator ruling, for each of the QUIC keys, the adapter port and the provider, when they land in pe-net:
- **Their own tests:**
  - the RFC 9001 vectors (header protection A.2, retry tag A.4, the Initial keys);
  - negative tests: a wrong key, a tampered packet, a replayed packet.
- **More tests** [I; the fuzzing and the last four from @design-reviewer]:
  - Wycheproof AES-GCM vectors;
  - a handshake against a known-good QUIC implementation, in a dev-only test outside the shipped graph;
  - fuzzing of packet parsing and the handshake (cargo-fuzz, gate tier 2);
  - that the joiner sends nothing above the transport before the welcome;
  - a QUIC key update, and the Retry token;
  - that secrets are zeroized on drop;
  - that an invite or reply code with non-ASCII or malformed text is an error, never a panic (the M0 parser
    panicked on non-ASCII);
  - that no path in the glue unwraps on a peer's input: M0's retry tag and header-key wrappers did, as quinn's
    own adapter does, and pe-net returns errors instead [@chunks-reviewer's Lows].
- **Reviewer:** @design-reviewer, not the author.

### 2.6 What M0 measured (for scale only)

Each figure is from one run on one machine, at idle priority with other work running [V, the M0 report's
table]:

| What | Measured |
|---|---|
| Handshake | 2-5 ms (loopback, and between Windows and WSL Linux) |
| Small round trips (32-1,024 B), datagrams or a stream | 0.06-0.32 ms at p50 |
| One stream | 159-339 MB/s |
| Datagram round trips during a bulk transfer | rise to 0.6-1.7 ms at p50 |
| Largest datagram payload, default path MTU | 1,162 B |

The rise under load comes from the two kinds of traffic sharing one connection's congestion window. The rate
budget and the priority between lanes decide it (section 4) [I].

## 3. Seam S4: lanes and delivery

### 3.1 The interface

The shape is the plan's (appendix A) and @engine-architect's seams list: lanes protocol, action, pose, repair and
bulk, in that priority; pe-diverge's four repair messages go on the repair lane [I for the rest].

```rust
pub type PeerId = [u8; 32];                    // the peer's Ed25519 public key (2.2)
pub enum Lane { Protocol, Action, Pose, Repair, Bulk }   // priority order

pub trait Transport {
    fn send(&mut self, to: PeerId, lane: Lane, bytes: Bytes);  // never blocks; see 3.2 for what each lane promises
    fn poll(&mut self) -> Option<Received>;                    // Received { from, lane, bytes, at: HostTime }
    fn link(&self, peer: PeerId) -> LinkStats;                 // 4.4
    fn events(&mut self) -> Vec<NetEvent>;                     // link up, degraded, down, saturated; join requests (6)
}
```

pe-agree, pe-diverge and pe-world give pe-net opaque bytes and never see QUIC. pe-net never decodes a payload.

### 3.2 What each lane promises

| Lane | Carries | Delivery | Over QUIC |
|---|---|---|---|
| Protocol | pe-agree's messages: proposals, votes, cuts, joins and leaves | Reliable, each message once, any order | Datagrams, repeated until acknowledged (3.3); a large message on its own stream |
| Action | signed entries: inputs, hand entries, edits, governance | Reliable, each message once, any order | As Protocol, below it in priority |
| Pose | display-only poses that are not entries (a remote head and body between entries) | Newest only: an unsent pose is replaced by the next, never queued; a late one is dropped | Datagrams, never repeated (3.4) |
| Repair | pe-diverge's NodeRequest/Reply, RecordRequest/Reply | Reliable, each message once, any order | One unidirectional stream per message (3.5) |
| Bulk | blobs fetched by hash; state for a newcomer | Reliable, each message once, any order | One unidirectional stream per message, lowest priority |

Rules for all lanes [I]:
- **No order.** pe-net promises no order between messages, on any lane. Receivers order by what the bytes say:
  a chain's sequence number, a cut's height. That's what lets Protocol and Action use repeated datagrams, and it
  keeps a loss on one message from holding up another (no head-of-line blocking).
- **Once.** The receiver drops duplicates, so each message is delivered to the caller once.
- **Per link.** Everything is per peer pair: one connection, one budget (4), one set of queues. A slow peer slows
  only what is sent to it.
- **On a link going down,** the messages still queued for it are dropped, and the caller gets `LinkDown`. pe-agree
  recovers what the other side lacks through its own protocol, as it must anyway for a peer that was away.

### 3.3 Protocol and Action: reliable small messages over datagrams

Each message is sent once, with a little bounded redundancy so a single lost datagram usually costs nothing,
and retransmitted only after a timeout. Repeating every unacknowledged message in every datagram (draft 1)
would multiply bandwidth by the datagrams sent per round trip, not by the loss rate: about six times at 60
datagrams/s and a 100 ms RTT. On a thin link, new messages would then wait behind repeats of old ones, which is
R18's failure again [@design-reviewer's High].
- **Numbering:** pe-net numbers each message per peer and per lane: `seq` = 0, 1, 2, ...
- **Acknowledgements:** every datagram to a peer carries, per lane:
  - the highest seq received in order from that peer;
  - up to `N_ack_ranges` ranges received after it, as QUIC's ACK frame does.

  Ranges rather than a fixed bitmap, so a full second of messages in flight still fits.
- **First send:** a new message goes in the next datagram to the peer, Protocol before Action.
- **Bounded redundancy:** a message of at most `L_redundant` bytes also rides in the next `r - 1` datagrams to
  that peer (`r` = 2). This is rollback netcode's way with its tiny inputs: one lost datagram then costs no round
  trip. A ride only ever takes space that would otherwise go unused [@shader-lang, found building lanes.rs; V]:
  - **piggyback only:** it rides in a datagram that goes out anyway, for a new message, a retransmission, a
    pose or a due acknowledgement, after all of those. pe-net never sends a datagram for rides alone;
  - **only on a link with room:** rides are optional bytes, like Repair and Bulk, and stop when the link shows
    it is full (4.2's ride gate: the delay guard and a budget check). Every byte then goes to new data and
    retransmissions.

  Without these two rules, the lanes test's 16 kbit/s cell failed: rides took a thin link's capacity, as the
  repeats did in draft 1.
- **Retransmission:** a message still unacknowledged after `RTO = srtt + max(4 * rttvar, T_rto_min)` goes again
  in the next datagram, and the timer doubles on each further loss (as QUIC's PTO, RFC 9002).
  - New messages and retransmissions share their lane's queue, oldest first.
  - Every byte counts against the budget (4).
- **Acknowledgement-only datagrams:** a peer with nothing to send sends one within `T_ack` of receiving.
- **Large messages:** a message larger than half a datagram's payload goes on its own unidirectional stream
  instead, at the stream priority of its lane. That covers, for example, an edit carrying a small simulation blob
  (up to 1 MiB, `docs/spec/world-model.md`).
- **The cost:** wire bytes over payload bytes stay near `1 + (r - 1) × (share of small messages) + loss` while
  rides are on, and near `1 + framing + loss` on a thin link, where they're off. Both counts are pe-net's: its
  datagram bytes over its message bytes. At the IP layer, 30-byte actions can never come near 1.5: each packet
  adds about 57 B of IP, UDP, QUIC header, AEAD tag and frame header [I: 28 + about 11 with our 8-byte
  connection ids + 16 + 2]. Section 9 tests that it stays under 1.5 with 1 % loss, over quinn on a thin shaped
  link.
  So far that's shown only with the budget handed to the lanes: 1.30, at 2,000 B/s of pe-net datagram bytes,
  with a 30-byte action every second frame at 60 Hz and rides off [V `pe-net/src/lanes.rs`, budget set to the
  link's capacity].

### 3.4 Pose: newest only

- **One slot per peer:** `send(peer, Pose, ...)` replaces any pose not yet sent to that peer. A caller with
  several pose sources (head, two hands) packs them into one message per frame.
- **When it's sent:** only when the budget has room after Protocol and Action. A pose is never repeated.
- **On arrival:** a pose older than the last one delivered from that peer (by its seq) is dropped.
- **Under a tight budget,** poses thin out on their own (fewer per second). That's continuous by construction,
  with no levels (A3 of review file 01).

### 3.5 Repair and Bulk: streams

- **Each message** is one unidirectional QUIC stream: written, then finished. The receiver delivers it when the
  stream ends.
- **Priority:** Repair above Bulk (quinn's stream priority), both below datagram traffic in pe-net's queue (4.2).
- **Limits:**
  - at most `N_streams_in` incoming streams open at once per peer (QUIC flow control);
  - at most `L_bulk` bytes per message. Larger transfers are split by the caller, for example by hash ranges of a
    blob's chunks.
- **A request and its reply** are two messages. pe-diverge matches them by content (the root), as its spec says,
  so pe-net needs no request ids.

### 3.6 Framing

- **A datagram:** a 1-byte format version, the acknowledgements, then items `(lane, seq, length, bytes)` with
  varint fields, then optional pe-net control records (clock samples, 5.2; rendezvous, 6.7).
- **A stream:** a 1-byte kind, then its bytes to the stream's end. The kinds are:
  - a lane (0-4), followed by a varint seq and the message;
  - the hello (`0x80`), the welcome (`0x81`) and the up record (`0x82`) of 6.3-6.4, each bounded (4 KiB, 1 MiB,
    empty).

  Any other kind, a Pose on a stream, or a stream over its bound is a protocol fault (code 6).
- **Limits:**
  - **The datagram payload** is QUIC's `max_datagram_size()`: at least 1,162 B at QUIC's minimum path MTU
    [V M0], more after path-MTU discovery.
  - **One message:** up to `L_msg` on Protocol and Action, `L_bulk` on Bulk.
  - **Anything over a limit** is a `Fault` naming the peer, never a panic (the plan's "no panics on a peer's
    input").

## 4. The rate budget v0

It replaces R18's level ladder (D56) and its delay-gradient estimator, which were tuned on an oracle signal
(review file 01, defect 10).

### 4.1 What it reads

Only what a real endpoint observes [the plan's rule against oracles]: QUIC's congestion window and its
smoothed and minimum round-trip times, lost packets and congestion events, all from quinn-proto's path
statistics. The harness reports nothing else to it.

### 4.2 The budget and the queue

- **The congestion controller is QUIC's:** quinn's Cubic by default. It handles pacing, loss recovery and
  congestion response, with loss as a signal as well as delay, so fq_codel and CAKE routers are covered
  (defect 10's gap) [S quinn-proto; I].
- **pe-net's own queue, per link:** on every wake-up, pe-net hands quinn only what the controller lets out now,
  in priority order:
  1. Protocol;
  2. Action;
  3. the newest Pose;
  4. Repair;
  5. Bulk.

- **The budget:** `cwnd / srtt` bytes per second, from quinn-proto's public path statistics (`cwnd`, `rtt`).
  quinn-proto keeps its bytes-in-flight count private [S `connection/mod.rs`, `pub(crate) fn
  bytes_in_flight`].
- **Priority holds only if quinn's buffers stay small** [@design-reviewer's Medium 2]. pe-net relies on quinn's
  own backpressure:
  - **Datagrams:** `datagram_send_buffer_size` is set to about `N_dgram_buf` datagrams. pe-net hands over a
    datagram only when `send_buffer_space()` has room for it [S `connection/datagrams.rs`], so quinn never drops
    an old datagram to make room.
  - **Streams:** `send_window` (bytes accepted from the application across all streams [S
    `config/transport.rs`]) is set to `W_send`. pe-net writes Repair and Bulk messages in chunks of at most
    `L_chunk`, each gated by the budget, and a write that quinn blocks waits in pe-net's queue. So a 1 MiB
    message never lands in quinn's buffer at once.
  - **Inside a packet,** quinn-proto writes DATAGRAM frames before STREAM frames [S `connection/mod.rs`,
    `populate_packet`]. So Protocol, Action and Pose datagrams already handed to quinn go out ahead of any stream
    data it holds.
  - **Between Repair and Bulk,** quinn's stream priority decides.
- **The ride gate:** rides (3.3) are optional bytes, so they need a sign that the link has room. `cwnd / srtt`
  alone isn't one: quinn's Cubic doesn't grow the window while the sender is application-limited (quinn-proto
  0.11.19 `cubic.rs`, `on_ack`), and even its 2,400-byte minimum over an 80 ms srtt reads 30,000 B/s, fifteen
  times a 16 kbit/s link (2,000 B/s) [@design-reviewer's Medium on draft 1.3; S]. Rides are on only while both hold:
  - **the delay guard (4.3):** `srtt - base_rtt` is under `D_guard`. Once it reaches `D_guard`, rides stay off
    until it is back under `D_guard / 2`. Without that band they would flap: rides build the queue, which stops
    them, which drains it [I]. This catches a thin link once its queue reaches about `D_guard`;
  - **the budget check:** the budget is at least `K_redundant` times the reliable send rate. That covers a link
    whose congestion response has already cut `cwnd`. The rate counts wire bytes (each message with its
    framing, as the budget does) of Protocol and Action messages first sent, over a one-second window that
    slides and updates at each first send. So at the start of a burst, rides add at most about
    budget / `K_redundant` [I]. With `r` = 2, rides at most double the small messages' bytes, so a factor of 4
    leaves half the budget for poses, retransmissions and the low lanes [I].
- **What the gate doesn't see:** an AQM router (fq_codel, CAKE) keeps the queue short and drops instead, so the
  delay guard may never fire, and random loss looks like congestion loss. The loss-only cells of section 9
  measure what rides cost there; for M1 that's enough [@design-reviewer]. **Exit (open, N5):** if those cells
  show rides raising loss more than they save, a loss-aware rule comes before sessions with strangers, for
  example rides off for a few seconds to compare the loss.
- **Jitter:** on Wi-Fi, `srtt - base_rtt` often swings past `D_guard`, so the guard turns rides off where they
  help most. 9.7 measures the share of time rides are on, with a jitter cell; no rule change yet [I].

### 4.3 The delay guard for the low lanes

- **The problem:** a loss-based controller fills a deep router buffer before it backs off, which would add
  queueing delay to actions and poses.
- **The guard:** Repair and Bulk send only while `srtt - base_rtt` is under `D_guard`, so the low lanes yield
  before the queue grows. Redundant rides follow the same guard, with a band (4.2). This is LEDBAT's idea, applied to the low lanes only [I].
- **`base_rtt`** is pe-net's own minimum of the RTT samples over the last `W_base`. Without a window, a route
  change that raises the base delay would block Repair and Bulk for good [@design-reviewer's Low 3].
- **Measured in M1:** Cubic with the guard against quinn's BBR, on delay cells and on loss-only (AQM) cells
  (section 9).

### 4.4 A link that cannot carry its share

- **LinkStats** per peer: state (6.5), `srtt`, `min_rtt`, the budget in bytes per second, loss rate, the bytes
  queued per lane, and the age of the oldest unacknowledged Protocol and Action messages.
- **`LinkSaturated`** fires when the oldest unacknowledged Action message is older than `T_saturated`; `LinkOk`
  fires when that clears.
- **pe-net only reports.** It removes and demotes nobody, so what to do with a slow member is pe-agree's and
  the user's rulings' business [I; R18 and the trust survey].
- **Poses** thin out before anything else (3.4).

## 5. The clock

It keeps the old fault-tolerant midpoint and drops the frequency loop, converges before frame 0, then steers
gradually (the plan; review file 01; item clock-sync-frequencies-converge-before-frame-0-s). Every limit is in
seconds (the user's ruling md4b42d16d9df: the tick rate is configurable per world).

### 5.1 Three times

- **`HostTime`:** this machine's monotonic clock, in i64 nanoseconds (seam S5). Never adjusted.
- **Session time:** `HostTime + offset`. The offset is the clock's only state, and it changes only as 5.4 and
  5.5 allow.
- **The frame:** `frame(t) = floor((session(t) - epoch) * rate)`, where `epoch` (frame 0's session time) and
  `rate` (the world's tick rate) come from the world, in the welcome.

### 5.2 Samples

- **Exchange:** each side sends a clock record to each peer every `T_sample`, every `T_sample_fast` while
  converging. These are pe-net control records in datagrams (3.6), never repeated, and ahead of every lane.
- **Four timestamps, as NTP:**
  - t1: my send, in HostTime;
  - t2: the peer's receive, in its session time;
  - t3: the peer's send, in its session time;
  - t4: my receive, in HostTime.

  Then the offset `θ = ((t2 - t1) + (t3 - t4)) / 2` (the peer's session time minus my HostTime), and the delay
  `δ = (t4 - t1) - (t3 - t2)`. The peer's time to answer drops out.
- **Filter** (kept from pe-app): samples from the last `W_clock`, and per `B_clock` bucket the one with the
  smallest δ, since a queued sample is biased by its queue. A peer's offset is the θ of the smallest-δ sample in
  its newest three buckets. Its error is within δ/2.

### 5.3 The target: the fault-tolerant midpoint

- **The values:** each connected member's θ, plus my own offset.
- **The midpoint:** n values, t = floor((n - 1) / 3). Drop the t lowest and the t highest, and take the
  midpoint of the lowest and highest that remain.
- **Why this one:** with n ≥ 3t + 1, every honest target stays inside the honest range, and the honest spread
  at least halves per round, even against a member that tells each peer something different. A median doesn't
  have that property (review CM-1 of pe-app's clock).
- **With two members,** t = 0, so each moves to the middle.
- **At two and three members,** t = 0, so the clock tolerates no liar: one member can pull the target where it
  likes [@design-reviewer's Low 5]. That costs fairness, not safety. Frame stamps only bound back-dating, voids
  are decided at cuts, so a liar mostly hurts its own stamps [I].

### 5.4 Before frame 0: converge

- **A joiner** sets its offset to the target at once, each round, while it is not yet in the session. A jump is
  allowed here, since nothing has run.
- **The target comes from the members, not from the inviter alone** [@design-reviewer's Medium 3].
  - Before the welcome, the joiner's only peer is the inviter, so one peer's word would set its clock.
  - So the joiner first connects to the members (6.7), and its target is the midpoint over the members it has
    samples from.
  - It needs samples from at least a quorum (two thirds, rounded up) of the members on the list it starts from
    (6.4), and from every one of them it can reach within `T_ready`.
- **It is clock-ready** when that target has moved less than `E_ready` for three rounds running. It then tells
  pe-agree, and pe-agree's join waits for that.
- **The order:** the welcome, then the mesh, then the samples, then clock-ready, then pe-agree's join.
- **The quorum counts the start cut's list,** not the welcome's hint (6.4). If the cut's list differs from the
  hint, the joiner connects to the members it was missing and re-checks its quorum against the cut's list
  before it acts. So a hint that leaves members out can't make it ready on too few [@design-reviewer's Low on
  draft 1.1].
- **If it isn't ready within `T_ready`,** that is a `ClockFault` (5.6), and the join fails with its reason.
- **A world's first member** has offset 0, and is ready at once.

### 5.5 After frame 0: steer

- **No jumps after frame 0:** the offset moves toward the target at a rate
  `clamp((target - offset) / T_settle, -S_max, +S_max)`.
- **Bounded:** with `S_max` = 0.2 %, session time runs between 0.998 and 1.002 of real time. It never stops,
  never runs backwards and never skips a frame. At 60 Hz a frame's period changes by at most 33 µs.
- **Drift without a frequency loop:** a steady drift d between two machines leaves a steady error of about
  `d * T_settle`. That's 0.4 ms at 100 ppm, typical of PCs, and 4 ms at the 0.1 % fault limit [I, arithmetic].
  That's why the frequency loop can go.

### 5.6 Clock faults

**The rule** (review file 01, accepted into the plan): a clock that runs more than 0.1 % away from the members'
midpoint is a fault on its machine.
- **Detection:** the slope of a peer's θ over `W_clock`, or my own steering pinned at `S_max` for longer than
  `T_pinned`.
- **Who is at fault:**
  - with three or more members, it's the one outside the midpoint;
  - with two, nobody can tell which, so both are told how far apart they are.
- **What it does:** a `ClockFault` event goes to pe-agree and the app. pe-net doesn't drop the link; what
  follows is pe-agree's (open question N2).
- **This machine's own case:** pe-app measured the WSL2 VM's clock drifting 2-9 % from Windows
  [`crates/pe-app/src/clock.rs` header, `spikes/out/m1/wsl-clock-skew`]. Under the rule, a Windows-WSL session
  on one machine is a clock fault by design.
  - So real-time cross-OS tests run between two machines or on hosted runners.
  - The harness uses virtual time and is unaffected.
  - QUIC itself worked over that link in M0 [V].

### 5.7 What the simulation thread reads

pe-net publishes a small model for the simulation thread to read without a lock: `(host_ref, offset_ref,
slew_rate, epoch, rate)`, swapped atomically. The simulation thread computes `session(now)`, `frame_at(now)` and
`host_time_of(frame)` itself, so asking for the time never crosses a thread. pe-agree's `tick(HostTime)` gets
its frame this way (seam S3).

## 6. The connection lifecycle

### 6.1 The endpoint and its addresses

- **One UDP socket per member,** on a fixed port (chosen once, kept in the member's settings) so that invites
  stay valid. It is dual-stack where the system allows, otherwise one socket per address family. All
  connections share it, and simultaneous open needs that: the punch packets leave from the port the peer will
  answer.
- **An invite's addresses:** every non-loopback, non-link-local interface address (LAN and VPN), with the
  socket's port, plus any the user adds. A reply code carries the joiner's the same way.
- **Formats:** `pe1:` for an invite, `pe1r:` for a reply code. Both have a version, an expiry, and a checksum
  against typing errors. Both are checked as ASCII before parsing, and any malformed text is an error, never a
  panic (2.5).

### 6.2 Dialling

- **Staggered:** the joiner dials the invite's addresses `T_stagger` apart, all with the pinned key. The first
  handshake confirmed wins, and the rest are closed.
- **The reply code:** after `T_reply` with no answer, the joiner shows one (2.4). When the inviter takes it in,
  both send at once: the inviter's punch packets and its own dial to the joiner's addresses, the joiner's dials
  to the invite's.
- **Two connections at once** [@design-reviewer's Medium 1]: when both sides dial, there are two QUIC
  connections, A's to B and B's to A.
  - Keeping whichever is confirmed first is a local judgement: each side could see a different one first, and
    both would be closed.
  - So both sides keep the connection whose QUIC client has the lower public key (bytes compared), whichever
    finished first, and close the other. Every rendezvous (6.7) uses the same rule.

### 6.3 Hello and the connection guards

- **First, the hello:** after the handshake is confirmed, each side's first message, on a control stream, is a
  hello:
  - the protocol version;
  - the engine build id;
  - the digest and time of its physics self-test;
  - from M2, each module's hash;
  - the line it means: the `LineId` of `docs/spec/world-model.md` r9 (called the world id in earlier drafts);
  - the invite id or token;
  - its addresses;
  - whether its clock is ready (5.4).
- **The guards, in order, on both sides:** protocol version, build id, self-test digest, line id (R-G1 of
  `docs/spec/divergence.md`).
  - The first difference refuses the connection, with a reason code and the two values, so the joiner is told
    why before it joins (the plan's M1 acceptance).
  - The joiner checks the other side's hello too, and refuses on a mismatch, rather than learning of it only
    from a close [@design-reviewer's Low 4].
- **Then admission:** pe-net raises `JoinRequest { peer, hello }`, and the app decides from the agreed governance
  (2.3) by calling `admit(peer, welcome)` or `refuse(peer, reason)`.

### 6.4 The welcome

- **It carries:**
  - the line id;
  - the epoch and the tick rate (5.1);
  - the members, with their keys and last addresses, for 6.7;
  - what pe-agree needs to start the join (pe-agree's to define).
- **The member list is only a hint** [@design-reviewer's Low 1]. The list that counts comes from the cut the
  joiner starts from, checked against that cut's root, so an inviter can neither hide a member nor add one. The
  joiner uses the welcome's list only to start connecting, and re-checks it against the cut.
- **Until the welcome arrives,** the joiner sends only its hello and clock records (2.3).
- **The exchange** [found building the Core; V `pe-net/tests/core.rs`]: lane traffic is closed both ways until
  it completes. Each side opens so that nothing is sent before the other side takes it in, because QUIC doesn't
  order one stream against another, and a lane message on a stream, once delivered, isn't sent again.
  - **The role** comes from the app (`set_member`), fixed per connection when it is confirmed: a **member** admits
    (raises `JoinRequest`), a **joiner** waits to be welcomed. Never from what the peer sends.
  - **A member opens nothing before its own `admit`** [@design-reviewer's Medium on draft 1.6]. Admission (2.3)
    is the app's decision, so a peer that passes the hello guards (none of them secret) must not get lane
    traffic in or out before it. On `admit`, a member sends the welcome and opens its receive side.
  - **A joiner** that gets a welcome opens both sides and answers with an **up record**, on which the member
    that admitted it opens its send side.
  - **Two members** (6.7) each admit the other. Each opens its send side once it has both admitted and been
    welcomed: a welcome that arrives before its own `admit` is held. No up record is sent.
  - **A welcome that overtakes the peer's hello** is held until the hello arrives.
  - **The link is Up** when both sides are open. Lane traffic arriving at a side that is still closed is a
    protocol fault (code 6), since no correct peer sends it.
  - **`T_welcome`** [@design-reviewer's Low 1]: a connection not Up within `T_welcome` of confirmation is
    closed: with code 8 by a member whose app hasn't decided, with code 5 by a side still waiting to be
    admitted. Keep-alive PINGs (6.5) would otherwise keep it open for good.
- **What each side may receive** [@design-reviewer's Low 2]. Anything else is a protocol fault (code 6):

  | Record | A member may take it | A joiner may take it |
  |---|---|---|
  | hello | once, first | once, first |
  | welcome | once, after the hello; held until its own `admit` | once, after the hello |
  | up record | once, after its own `admit`, and only if it hasn't had the peer's welcome (joiners never send welcomes) | never |
  | lane traffic | once its receive side is open (its `admit`) | once its receive side is open (the welcome) |

### 6.5 Link states

- **The states:** Connecting, Confirmed (handshake confirmed), Welcomed, Up, Degraded (nothing heard for
  `T_degraded`), Down (idle for `T_idle`, or closed).
- **Keep-alive:** QUIC PING every `T_keepalive` when idle.
- **Address changes:** QUIC connection migration is on. A peer's new address is used only after QUIC's path
  validation, so a forged source address can't redirect a link.

### 6.6 Close reasons

These are the application error codes on a QUIC close. The joiner's app shows the reason.

| Code | Reason |
|---|---|
| 0 | left |
| 1 | different protocol version |
| 2 | different engine build |
| 3 | different physics self-test |
| 4 | another line (`LineId`) |
| 5 | not admitted (not a member, banned, or a bad invite token) |
| 6 | protocol fault (bad bytes; the event names the peer) |
| 7 | clock fault (5.6) |
| 8 | overloaded |

### 6.7 Connecting to the other members

- **The mesh:** after its welcome, a joiner connects to every member in the welcome's list. One connection per
  peer pair (plan; file 11).
- **The rendezvous:** a member already connected to both sends each side a rendezvous record (the other's key
  and addresses), and both send at once (2.4).
- **Each of these connections** has its own hello, guards and welcome, judged from the agreed governance (2.3).
- **M3** adds relaying through members, and hole punching through NATs.

## 7. Threads and the sans-IO core

- **`pe_net::Core`** holds everything above the socket: quinn-proto's endpoint and connections, the lanes, the
  budget, the clock and the lifecycle. It takes time and bytes as inputs and returns bytes and events [I;
  quinn-proto is built this way, S].

```rust
impl Core {
    pub fn new(cfg: Config, identity: Identity, seed: [u8; 32]) -> Self;     // seed: see 8
    pub fn handle_datagram(&mut self, now: HostTime, from: SocketAddr, data: &[u8]);
    pub fn handle_timeout(&mut self, now: HostTime);
    pub fn poll_transmit(&mut self) -> Option<Transmit>;                  // (to, bytes)
    pub fn poll_timeout(&mut self) -> Option<HostTime>;                   // &mut, as quinn-proto's is
    pub fn dial(&mut self, now: HostTime, invite: &Invite);               // the app checks the expiry
    pub fn take_reply(&mut self, now: HostTime, reply: &Reply);
    pub fn admit(&mut self, now: HostTime, peer: PeerId, welcome: &Welcome);
    pub fn refuse(&mut self, now: HostTime, peer: PeerId, reason: Refusal);
    pub fn poll_lifecycle(&mut self) -> Option<Lifecycle>;                // Welcomed, Refused, Fault
    pub fn set_member(&mut self, member: bool);                           // the role of 6.4, per connection
    // plus the Transport methods of 3.1, and the clock model of 5.7
}
```

- **The network thread** owns the socket and a Core. It waits on the socket until `poll_timeout`, feeds what
  arrives, and sends what Core returns. Its time is the app's host clock (S5), passed in [V
  `pe-net/tests/thread.rs`, real UDP on loopback].
  - **Waking it:** the simulation thread's handle queues a command, then sends an empty datagram to the thread's
    socket from a loopback socket of its own. The Core never sees empty datagrams.
  - **After a send,** the Core's `poll_timeout` is now, so a new message never waits for an unrelated timer.
  - **Link statistics** come back as messages every 100 ms rather than through a shared lock, and the handle's
    `link` returns the latest it has taken in.
- **To and from the simulation thread:** bounded single-producer queues for outgoing `(peer, lane, bytes)` and
  incoming `Received` and events, plus the clock model of 5.7.
- **The simulation thread never waits on the network.** A full outgoing queue is a local fault, reported, not a
  stall. The queue is sized for `T_queue` of traffic.
- **The other threads** (render, XR, agent handlers) are WS-app's to arrange with WS-state, WS-render and WS-xr
  (the plan). pe-net needs only its own thread.

## 8. What the harness drives

- **Exactly the production path:** the harness drives Core directly, with virtual time, so every scenario runs
  pe-net's real lanes, budget and congestion control. That is the plan's rule against oracles.
- **Determinism:**
  - quinn-proto's endpoint takes a fixed RNG seed (`EndpointConfig::rng_seed`) [S];
  - our provider's randomness (key shares, nonces) comes from the same seed through a test-only source;
  - time is the harness's;
  - so a scenario is a pure function of its seed. Shown in `pe-net-crypto/tests/quic.rs`: a seeded handshake
    repeats byte for byte [V].
- **What the harness must install for that** (requirements on pe-harness, @design-reviewer's ruling on
  pe-net-crypto) [S quinn-proto 0.11.19, V]:
  - quinn-proto's default connection-id generators and the client's `initial_dst_cid_provider` draw on
    `rand::rng()`, the thread's generator, outside any seed. Install `pe_net_crypto::quinn::Cids` for both:
    `EndpointConfig::cid_generator` and `ClientConfig::initial_dst_cid_provider(Cids::initial_dcid(rng))`;
  - address-validation tokens carry a wall-clock time. Set a fixed `ServerConfig::time_source`;
  - the seeded source is the `insecure-deterministic-rng` feature of pe-net-crypto. The gate checks that no
    shipped binary enables it (@provenance-eng's "shipped builds" check).
  - **Shipped binaries are built per crate** (`cargo build -p pe-app --release`), never in a workspace-wide
    build. Cargo unifies features across every member a build selects, so with pe-harness in the same build the
    seeded source would reach pe-app's binary [S cargo's feature unification; @provenance-eng].
- **The network model (WS-harness)** per link: one-way delay, seeded jitter, random and burst loss, a bottleneck
  with drop-tail or AQM queueing, reordering, and an "inbound blocked" switch for the firewall of 2.4.
- **The demo's CI run** also uses real QUIC between two processes on one machine (the plan's M1 acceptance).

## 9. Tests for M1

Gated, unless marked:

1. **Lanes:**
   - a Pose is superseded, never queued;
   - Protocol and Action arrive once each at 0-30 % random loss, and at 10 % loss most need no retransmission
     round trip (each counted; that cell runs with budget to spare, so rides are on);
   - nothing is delivered twice;
   - the ride gate (4.2), over quinn on shaped links (harness cells, the real budget path). Each cell sends one
     30-byte action every second frame at 60 Hz and nothing else, with 1 % loss: about 30 packets/s of about
     100 IP bytes, and about 134 with a ride [I, arithmetic; @design-reviewer]. Link rates count IP bytes, as a
     shaper does. Each cell stays at least 10 % away from a full link with rides on, so it comes out the same
     way every run. Each reports its measured load, so a reader can tell "rides overfilled it" from "the cell
     overfilled it":
     - **thin, the gate must close:** 28 kbit/s (3,500 B/s), about 86 % without rides and 115 % with them.
       Gated on pe-net datagram bytes over message bytes under 1.5 (3.3);
     - **room, the gate must stay open:** 64 kbit/s (8,000 B/s), about 38 % without rides and 50 % with them.
       Gated on rides being on for at least 90 % of the run. The ratio is only reported (about 2, by design).
       This catches a guard that starves rides on a link with room;
     - **the queue, in both:** `srtt - base_rtt` at p50 and p99, failing above `D_guard + 10 ms + s` at p99,
       where `s` is one serialization time of the cell's largest packet at the link's rate (about 30 ms for
       100 B at 3,500 B/s; measured from the packets sent, not assumed). So no cell passes on the ratio with a
       deep queue;
   - under a budget too small for everything, Protocol is never delayed by the lower lanes.
2. **The budget:**
   - a thin link to one peer doesn't slow traffic to the others;
   - Repair and Bulk yield under the delay guard;
   - during a bulk transfer, action latency stays within `min_rtt + D_guard + 10 ms` at p99 (a harness cell).
3. **The clock:**
   - convergence before frame 0 within `T_ready`, to within `E_ready`;
   - the steering rate never exceeds `S_max`;
   - session time is monotonic;
   - with four members, one telling each peer something different stays bounded;
   - a clock 0.15 % off raises `ClockFault`.
4. **The lifecycle:**
   - a wrong key fails;
   - each guard refuses with its reason, and the joiner shows it;
   - a close before the welcome counts as a refusal;
   - the joiner sends nothing above the transport before the welcome;
   - with the inviter's inbound blocked (a headless run), the two-way invite connects (the fallback, 2.4);
   - by hand, on Windows: after the user really declines the prompt (block rules left behind), the two-way
     invite still connects, and pe-app shows how to undo the block;
   - by hand, once per checkpoint on the two physical machines: after the user accepts Windows' allow prompt,
     the joiner's dial connects directly (the default, 2.4);
   - a third member joins through a rendezvous;
   - when both sides dial at once, both keep the same connection (6.2);
   - the joiner refuses an inviter whose hello differs (6.3);
   - a joiner is not clock-ready on the inviter's word alone at four members (5.4);
   - a welcome whose member list differs from the start cut's is caught (6.4);
   - an address change survives, and a forged source address doesn't redirect a link.
5. **The security code:** 2.5's list.
6. **No panic** on any bytes from a peer (fuzzing, gate tier 2).
7. **Measured and reported, not gated:**
   - handshake time;
   - action latency over 20 seeds by 5 delays, against the user's target that the other machine sees an action
     within 0.25 s on good links (meef4f248923b);
   - Cubic with the guard against BBR, on delay cells and loss-only cells;
   - on the loss-only (AQM) cells, what rides cost and save, with rides on against off (4.2): the loss rate
     and the number of actions that needed a retransmission round trip. The net of the two decides;
   - the share of time rides are on, including one jitter cell (Wi-Fi-like `srtt` swings past `D_guard`, where
     rides help most), to learn whether the band of 4.2 is too tight.

## 10. Parameters

Every one is a starting value to measure against [I], in seconds or bytes.

| Name | Value | Meaning |
|---|---|---|
| `T_ack` | 10 ms | Longest wait before an acknowledgement-only datagram |
| `N_ack_ranges` | 32 | Acknowledgement ranges per lane in one datagram |
| `r` | 2 | Datagrams a small message rides in, the first send included |
| `L_redundant` | 128 B | Largest message that gets redundancy |
| `K_redundant` | 4 | Budget over reliable send rate (both in wire bytes) below which rides stop |
| `T_rto_min` | 20 ms | Floor of the retransmission timeout's variance term |
| `N_dgram_buf` | 4 | quinn's datagram send buffer, in datagrams |
| `W_send` | 64 KiB | quinn's `send_window`: stream bytes it may hold |
| `L_chunk` | 16 KiB | Largest Repair or Bulk write handed to quinn at once |
| `W_base` | 30 s | Window of `base_rtt` for the delay guard |
| `L_msg` | 1 MiB | Largest Protocol or Action message (an edit with a small simulation blob) |
| `L_bulk` | 64 MiB | Largest Bulk message |
| `N_streams_in` | 64 | Incoming streams open at once per peer |
| `D_guard` | 25 ms | Queueing delay above `base_rtt` at which Repair, Bulk and rides pause (rides resume below half) |
| `T_saturated` | 1 s | Oldest unacknowledged Action message before `LinkSaturated` |
| `T_sample` / `T_sample_fast` | 1 s / 0.2 s | Clock sample interval, steady / converging |
| `W_clock` / `B_clock` | 30 s / 2 s | Clock sample window / bucket |
| `E_ready` | 1 ms | Convergence before frame 0 |
| `T_ready` | 5 s | Longest convergence before a `ClockFault` |
| `T_settle` | 4 s | Steering time constant |
| `S_max` | 0.2 % | Steering rate limit |
| `T_pinned` | 10 s | Steering pinned at `S_max` before a `ClockFault` |
| `T_stagger` | 0.25 s | Gap between dials to an invite's addresses |
| `T_welcome` | 10 s | Longest from confirmation to Up before the connection is closed (6.4) |
| `T_reply` | 3 s | No answer before the joiner shows a reply code |
| `T_keepalive` | 0.5 s | QUIC PING when idle |
| `T_degraded` | 1 s | Nothing heard before Degraded |
| `T_idle` | 5 s | QUIC idle timeout: Down |
| `T_queue` | 1 s | Outgoing queue capacity, in traffic time |

## 11. Open questions

- **N1. The firewall default** (2.4): settled by the user (m4534df91cc48). Windows' allow prompt is the default,
  and the two-way invite is the fallback.
- **N2. What a clock fault does** (5.6): refuse the join, keep the member but mark its frames, or only report.
  The rule that a clock off by 0.1 % is a fault comes from the review. Its consequence is pe-agree's and the
  coordinator's.
- **N3. A saturated link** (4.4): pe-net reports it. What pe-agree does with a member that can't keep up falls
  under the user's R18 rulings, as file 01's proposals carry them into M3.
- **N4. Per-entry signatures against link authentication:** review file 01 proposes that the encrypted link
  authenticates the sender and only every Kth entry is signed. S1 signs every entry in M1. The proposal is the
  user's call, and not for M1.
- **N5. Redundant rides on AQM links** (4.2): the gate can't tell congestion loss from random loss. M1 measures
  it (9.7); a loss-aware rule comes before sessions with strangers if rides raise loss more than they save.

## 12. What pe-net needs from the others

- **WS-agree (pe-agree):**
  - accept messages in any order, delivered once (3.2);
  - give entry signatures their own prefix, and use `verify_strict` (2.2);
  - wait for clock-ready before a join (5.4);
  - define what the welcome carries for its join (6.4).
- **WS-seams:** S4 as in 3.1; `HostTime` as in S5.
- **WS-harness:** the network model of 8, its inbound-blocked switch, virtual time, and the seeded randomness.
- **WS-app:**
  - the network thread and the queues (7);
  - the invite and reply-code screens, and showing refusal reasons (6);
  - the admission call from governance (6.3).
- **WS-diverge:** repair messages within `L_msg`, matched by content (3.5).
- **WS-world:** blobs on Bulk within `L_bulk`, fetched by hash; small simulation blobs inside edit entries, on
  Action's stream path (3.3).
