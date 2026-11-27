# Primordial Engine: the friend-test kit (Linux)

A prototype build for one test session: two people in VR on a flat plane, spawning and throwing physics objects
together. This kit is the Linux side: Fedora with an NVIDIA GPU and a Quest 3 through WiVRn.

## Before the session

1. **The GPU driver:** any working Vulkan driver for your GPU: NVIDIA's proprietary driver, or Mesa's NVK. The
   program draws through Vulkan on the GPU WiVRn names, and shares it with WiVRn's encoder. If WiVRn already
   streams to your headset, this is done.
2. **WiVRn:** running, with the headset connected and showing WiVRn's lobby.
3. **The network:** both machines must reach each other, for example both on the same Tailscale network.
4. **Sound:** collisions play spatial sound through Valve's Steam Audio (`libphonon.so`, with its licence and
   notices, is in the kit). It plays on the system's default output (PipeWire or PulseAudio, through ALSA) at
   48 kHz. Without a working output the program runs silent; `steam-audio.txt` in the logs says why.
5. **Unpack the kit:** `tar xzf pe-friend-kit-<version>.tar.gz && cd pe-friend-kit-<version>`.

Optional, without the headset: `./run.sh --headless --seconds 20` checks that the program starts on this machine.
Send the logs back afterwards (below).

`run.sh` opens the flat plane, which has the spawn panel. A `pe-app` started by hand without `--preset plane` opens
the ball pit instead, which has no spawn panel.

## Joining the session

The other player starts the session and sends you an **invite** (a line starting `pe1:`).

```
./run.sh --join '<the invite>'
```

Put the headset on first. The invite lets you in by itself: VR starts once you're in, usually within a few seconds.
The invite works for one player and for 60 minutes. A second player, an expired invite, or one from the other
player's earlier session (before they restarted) is refused, and the terminal says why: ask for a new invite.

In VR you can also join from the menu: copy the invite on this machine, then press **Join a session** (twice: the
first press says what happens). The game restarts, VR goes off for a few seconds, and comes back in their session.

It also prints a reply code (`pe1r:`). That's a fallback: send it only if the other player asks for it.

To quit, type `quit` and Enter in the terminal, or press Ctrl-C, or close the program. In VR, the menu button on your
left controller opens the menu; **Quit** asks once more before it quits. While the program waits to be let in,
`quit` and Ctrl-C end it too (it then ends with code 130, which isn't a failure).

If you open a session yourself and the other player's join times out, your firewall may be blocking it: run
`sudo firewall-cmd --add-port=4433/udp` and try again.

## Sending the logs back

```
./collect-logs.sh
```

It prints the path of one `.tar.gz` file; send it to the other player. Each run's logs, and a short report of this
machine's VR setup (GPU driver, Vulkan, the OpenXR runtime found, WiVRn, network), are in
`~/pe-friend-test/logs/<time>/`. Nothing is sent anywhere by the program itself, other than to the other player's
machine during the session.

## Known limits

- **After a network drop,** the other player comes back within about a second of the link returning, and the
  world keeps moving.
- **A drop longer than about 30 s ends the session:** start or join again.
- **If one machine runs slower than real time,** the game agrees on the shared world later on that machine, by
  however far behind it is. Nothing stops, and the other player keeps moving. A short hitch adds about 1.5 s and
  recovers in a few seconds.
- **Mesh colliders are off in this build.**
- **Throwing:** a throw leaves with your hand's real speed and direction, with no top speed. Only your grip strength
  (110 N) limits it, so a light ball goes as fast as you swing, and a heavy object leaves slower.
- **Walls and floors:** held and thrown objects can pass through them. A held ball pushed into the floor sinks about
  4 cm into it. Pushing your hand through a thin wall (2 cm) while holding a ball can pull the ball through. A throw
  is stopped by fixed walls up to about 50 m/s.
- **Two hands:** a two-hand grab is held by one hand at a time. When you let go with one hand, the object stays in
  the other.
- **Fast swings:** a held object swung into a light one can send it off at up to about twice the swing speed. Your
  empty hand still pushes things at 10 m/s at most.
- **The object limit is 52,000 collision shapes, 100 times the last test's,** for a stress test: near it, either
  machine may slow to a crawl, by design. Type `limit` in the game's window to see it, and `limit shapes <n>` to
  change it for both players at once (only the session's players may; 0 is refused).
- **An agent on this machine can drive the game:** pe-app's MCP server is on, reachable from this computer only
  (127.0.0.1), never from the network. Its address and key are in `friend-test/mcp.json` beside the identity file
  while the game runs; they are not in the logs.
- **Joining a crowded world:** a join with about 4,700 objects in the world took about 20 s. A join while the other
  machine is still spawning thousands of objects can time out: let the spawning finish (or Clear), then join again.

## If something goes wrong

- **"No OpenXR runtime" or the program opens without VR:** WiVRn isn't running or isn't registered as the active
  OpenXR runtime. With a Flatpak WiVRn, follow WiVRn's instructions for running non-Flatpak applications, or start
  with `XR_RUNTIME_JSON=<path to WiVRn's openxr_wivrn.json> ./run.sh ...`. The first lines `run.sh` prints show
  which runtime it found.
- **The join times out:** the two machines can't reach each other. Check that both are on the same Tailscale
  network (or LAN).
- **The other player's hands, or the balls they touch, look jumpy or late (more than before):** set
  `PE_COALESCE_TICKS=1` before starting, as in `PE_COALESCE_TICKS=1 ./run.sh ...`. It turns off pose-rollback
  coalescing; the default is 3.
- Either way, run `./collect-logs.sh` and send the file.
