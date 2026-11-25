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
the invite (also saved to a file, and printed); send it to the friend. When they send their reply code back, copy it
and press **Paste reply**. The spawn panel beside the menu makes boxes, spheres, capsules and stacks.

The left controller's menu button opens and closes the menu. Quit is in the menu, or type `quit` in this window.

Other ways: `-Window` (the desktop window, no VR), `-Headless` (no window, a check), `-Join '<invite>'` (join
someone else's session). Any other pe-app option passes through.

## The logs

Each run writes `%LOCALAPPDATA%\Primordial Engine\friend-test\logs\<time>\`: this machine's VR setup
(`system.txt`), pe-app's log, the cut log, the tick trace, the Steam Audio metrics (`steam-audio.txt`) and stderr.
