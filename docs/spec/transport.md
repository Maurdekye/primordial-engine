# pe-net: the transport

Status: draft 0.1, 2026-11-10, by @shader-lang (WS-net). Nothing in it is built. Draft 0.1 answers @design-reviewer's
review of draft 0 (Mediums 1-2, Lows 1-4).
- **Contents:** only the rules that M0's QUIC feasibility check settled (section 2), and the coordinator's
  rulings on that check (mc64f7a24509e).
- **Still to come:** the full specification of WS-net's M1 scope (`docs/refound-plan.md`, branch
  `docs/refound-plan`, b610464, the WS-net row). It is outlined in section 3.
- **Evidence:** the M0 report, `spikes/quic-feasibility/REPORT.md` (branch `spike/quic-feasibility`,
  6ff52ec), and its logs in `spikes/out/quic-feasibility/`.

Marks: **[V]** verified, with what was checked named; **[S]** read in a crate's source; **[I]** inference or
design judgement.

## 1. What pe-net is for

- **Connections:** direct QUIC connections between peers, made from an invite, with no helper nodes assumed
  (user Q9). Over a LAN or the user's own VPN in M1.
- **Two kinds of traffic on one connection per peer pair:**
  - unreliable datagrams, for live entries and poses;
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
  - **The first connection in M1, between two machines with no helper: a two-way invite** [I].
    - The joiner dials the invite's addresses first.
    - If nothing answers within 3 s, it shows a short reply code: its key, its addresses, and the invite's id.
    - The user pastes the code into the inviter, and both sides then send at once.
    - This keeps to the coordinator's preference for simultaneous open, and needs no change to either machine.
  - **The documented alternative:** Windows' own "allow access" prompt, the first time pe-app listens. It
    creates a firewall rule, but by the user's own click, which is the user's go the ruling asks for. Whether it
    becomes the default is the coordinator's and the user's call. M0's test ran under WSL and Hyper-V, where no
    prompt appears [I].
  - M3's hole punching inherits the same channels.
- **Seen in M0** [V `win-server-linux-client-punched.log`]: three small packets from the inviter's socket to
  the joiner's address let the Linux joiner's handshake through to Windows.
- **The other fix, not the default:** a firewall rule added at install changes the user's machine, so it needs
  the user's go (coordinator ruling).
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
  - that secrets are zeroized on drop.
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
budget and the priority between lanes decide it (section 3) [I].

## 3. Still to specify (the rest of the draft)

- The lanes on seam S4 (protocol, action, pose, repair, bulk), as @engine-architect's seams list has it. Which
  lanes are datagrams and which are streams; priorities; message framing and size limits.
- The rate budget v0 per link, and how it interacts with QUIC's congestion control. It replaces the old R18
  ladder.
- The clock: the fault-tolerant midpoint kept; the frequency loop dropped; converging before frame 0; gradual
  steering. The sans-IO interface pe-agree's `tick(HostTime)` uses.
- The connection lifecycle: invite, simultaneous open, handshake, welcome, the version and self-test
  comparison, keep-alive, path changes, and close with reason codes.
- The threading model: the network thread, and the handoff to the simulation thread.
- What the harness drives: quinn-proto with a deterministic network model.
