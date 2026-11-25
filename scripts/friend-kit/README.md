# Primordial Engine: the friend-test kit (Linux)

A prototype build for one test session: two people in VR on a flat plane, spawning and throwing physics objects
together. This kit is the Linux side: Fedora with an NVIDIA GPU and a Quest 3 through WiVRn.

## Before the session

1. **The GPU driver:** any working Vulkan driver for your GPU: NVIDIA's proprietary driver, or Mesa's NVK. The
   program draws through Vulkan on the GPU WiVRn names, and shares it with WiVRn's encoder. If WiVRn already
   streams to your headset, this is done.
2. **WiVRn:** running, with the headset connected and showing WiVRn's lobby.
3. **The network:** both machines must reach each other, for example both on the same Tailscale network.
4. **Unpack the kit:** `tar xzf pe-friend-kit-<version>.tar.gz && cd pe-friend-kit-<version>`.

Optional, without the headset: `./run.sh --headless --seconds 20` checks that the program starts on this machine.
Send the logs back afterwards (below).

`run.sh` opens the flat plane, which has the spawn panel. A `pe-app` started by hand without `--preset plane` opens
the ball pit instead, which has no spawn panel.

## Joining the session

The other player starts the session and sends you an **invite** (a line starting `pe1:`).

```
./run.sh --join '<the invite>'
```

It prints a **reply code** (a line starting `pe1r:`). Send it back to the other player; their machine lets yours in
once they paste it. Then put the headset on. The code is good for that one session only.

To quit, type `quit` and Enter in the terminal, or close the program. In VR, the menu button on your left
controller opens the menu (Quit, Leave).

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

## If something goes wrong

- **"No OpenXR runtime" or the program opens without VR:** WiVRn isn't running or isn't registered as the active
  OpenXR runtime. With a Flatpak WiVRn, follow WiVRn's instructions for running non-Flatpak applications, or start
  with `XR_RUNTIME_JSON=<path to WiVRn's openxr_wivrn.json> ./run.sh ...`. The first lines `run.sh` prints show
  which runtime it found.
- **The join times out:** the two machines can't reach each other. Check that both are on the same Tailscale
  network (or LAN).
- Either way, run `./collect-logs.sh` and send the file.
