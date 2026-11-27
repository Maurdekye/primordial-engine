# Primordial Engine: the friend test on Windows

The Windows side of the friend test: this PC opens the session in VR, and the friend joins from Linux (README.md).

## Before the session

1. **The headset's OpenXR runtime** is the active one (SteamVR, Virtual Desktop or the Oculus app). The first lines
   `run-windows.ps1` prints show which one it found.
2. **The firewall:** on the first run, Windows asks whether Primordial Engine may use the network. Allow it for
   the network type the launcher names (Private or Public) **before** putting the headset on. If you missed it, the friend's join times out: Windows
   Security, then Firewall & network protection, then "Allow an app through firewall". The launcher says so
   on every run while no allow rule for its `pe-app.exe` exists, or while a block rule does, and names your
   network's type (Private or Public). Each new kit's `pe-app.exe` is a new program to Windows, so it asks again.
   The session must go over a network the launcher lists as "allowed" (on this PC, Ethernet or Tailscale, both
   Private); a "blocked" Public network beside it is fine.
3. **The network:** both machines on the same Tailscale network (or LAN).
4. **Sound:** collisions play spatial sound through Valve's Steam Audio (`phonon.dll`, with its licence and
   notices, is in this folder), on Windows' default output device. Without one the program runs silent;
   `steam-audio.txt` in the logs says why.

## Starting

In PowerShell, in this folder:

```
powershell -ExecutionPolicy Bypass -File .\run-windows.ps1
```

It opens a session of your own on the flat plane, in VR, with the menu open. In the menu, **Invite a friend** copies
the invite (also saved to a file, and printed); send it to the friend. The invite lets them in by itself, for one
player and for 60 minutes: you'll see "<player> is joining with your invite…" and then "<player> joined the
session." in this window and in VR. The spawn panel beside the menu makes boxes, spheres, capsules and stacks.

If the friend runs an older build and sends a reply code (`pe1r:`), paste it into this window and press Enter.

The left controller's menu button opens and closes the menu. **Quit** in the menu asks once more before it quits, or
type `quit` in this window. After **New session** or **Join** in the menu, the program restarts itself, and this
window stays until the last run ends.

Other ways: `-Window` (the desktop window, no VR), `-Headless` (no window, a check), `-Join '<invite>'` (join
someone else's session). Any other pe-app option passes through.

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

## If something goes wrong

- **The other player's hands, or the balls they touch, look jumpy or late (more than before):** set
  `$env:PE_COALESCE_TICKS = 1` in the PowerShell window before starting. It turns off pose-rollback coalescing; the
  default is 3.

## The logs

Each run writes `%LOCALAPPDATA%\Primordial Engine\friend-test\logs\<time>\`: this machine's VR setup
(`system.txt`), pe-app's log, the cut log, the tick trace, the Steam Audio metrics (`steam-audio.txt`) and stderr.
