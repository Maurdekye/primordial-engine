#!/usr/bin/env bash
# Starts Primordial Engine in VR for the friend test, and records this machine's VR setup beside its logs.
#
#   ./run.sh                      VR, a world of your own on the flat plane (with the spawn panel)
#   ./run.sh --join <invite>      VR, joining the invite you were sent (it prints a reply code to send back)
#   ./run.sh --headless ...       no window and no VR (for a check without the headset)
#
# Any other pe-app option passes through. Each run's logs go to ~/pe-friend-test/logs/<time>/; send them with
# ./collect-logs.sh. Set PE_KIT_NO_VR=1 to leave out --vr.
set -u
here=$(cd "$(dirname "$0")" && pwd)
ts=$(date -u +%Y%m%dT%H%M%SZ)
logs=${PE_KIT_LOGS:-$HOME/pe-friend-test/logs}/$ts
mkdir -p "$logs"

# One OpenXR manifest: where it really is, its name and library, and the library path pe-app opens.
show_manifest() {
  local f=$1 real lib path
  if [ ! -f "$f" ]; then
    echo "$2: $f (does not exist)"
    return
  fi
  real=$(readlink -f "$f")
  echo "$2: $f -> $real"
  grep -E '"library_path"|"name"' "$f" | sed 's/^ */  /'
  lib=$(sed -n 's/.*"library_path" *: *"\([^"]*\)".*/\1/p' "$f")
  case "$lib" in
    "") return ;;
    /*) path=$lib ;;
    *) path=$(dirname "$real")/$lib ;;
  esac
  if [ -f "$path" ]; then echo "  library: present: $(readlink -f "$path")"; else echo "  library: MISSING: $path"; fi
}

# What this machine has for VR, before anything starts: the driver, the Vulkan device, the OpenXR runtime.
report() {
  echo "== run $ts, kit $(cat "$here/COMMIT" 2>/dev/null || echo unknown)"
  echo "== system"
  uname -srm
  grep -E '^(PRETTY_NAME|VERSION_ID)=' /etc/os-release 2>/dev/null
  echo "session: ${XDG_SESSION_TYPE:-?} desktop: ${XDG_CURRENT_DESKTOP:-?}"
  echo "== GPU driver"
  if command -v nvidia-smi >/dev/null; then
    nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>&1
  else
    echo "nvidia-smi: not found (no proprietary NVIDIA driver; Mesa's NVK, or another GPU)"
  fi
  cat /proc/driver/nvidia/version 2>/dev/null | head -1
  lsmod 2>/dev/null | awk '$1 ~ /^(nvidia|nvidia_drm|nvidia_modeset|nouveau|amdgpu|i915|xe)$/ {print "module:", $1}'
  echo "== Vulkan"
  if command -v vulkaninfo >/dev/null; then
    vulkaninfo --summary 2>&1 | grep -E 'Vulkan Instance Version|deviceName|driverName|driverInfo|apiVersion|deviceType' | head -20
  else
    echo "vulkaninfo: not installed (optional: sudo dnf install vulkan-tools)"
  fi
  echo "== OpenXR runtime"
  echo "XR_RUNTIME_JSON=${XR_RUNTIME_JSON-<unset>}"
  # The manifests in pe-app's order (pe-xr's loader.rs): XR_RUNTIME_JSON if set, else the first file under the XDG
  # config paths. "pe-app reads" marks the one it opens. The library line gives the path it opens: a relative
  # library_path resolves against the real file's folder, symlinks followed, as pe-app does since
  # @ticket:primordial-engine/pe-xr-resolve-a-manifest-s-relative-library-path (before it, run.sh said "present"
  # while pe-app looked beside the link and found nothing).
  local dirs="${XDG_CONFIG_HOME:-$HOME/.config}:${XDG_CONFIG_DIRS:-/etc/xdg}" d f tag="pe-app reads"
  # Set but empty counts as set, as in pe-xr (var_os): an empty path, a missing manifest (@design-reviewer's N1).
  if [ "${XR_RUNTIME_JSON+set}" = set ]; then
    show_manifest "$XR_RUNTIME_JSON" "$tag (XR_RUNTIME_JSON)"
    tag="not read (XR_RUNTIME_JSON is set)"
  fi
  local IFS=:
  for d in $dirs; do
    for f in "$d/openxr/1/active_runtime.$(uname -m).json" "$d/openxr/1/active_runtime.json"; do
      [ -f "$f" ] || continue
      show_manifest "$f" "$tag"
      tag="also found, not read"
    done
  done
  unset IFS
  [ "$tag" = "pe-app reads" ] && echo "no manifest: pe-app will say no OpenXR runtime is registered"
  echo "== WiVRn"
  command -v wivrn-server >/dev/null && echo "wivrn-server: $(command -v wivrn-server)"
  command -v flatpak >/dev/null && flatpak list --app 2>/dev/null | grep -i wivrn
  pgrep -a -f -i wivrn 2>/dev/null | cut -c1-120 | head -5 || true
  echo "== network"
  command -v tailscale >/dev/null && echo "tailscale: $(tailscale version 2>/dev/null | head -1)"
  [ -d /sys/class/net/tailscale0 ] && echo "tailscale0: present"
  if command -v firewall-cmd >/dev/null; then
    echo "firewalld: $(firewall-cmd --state 2>&1), zones: $(firewall-cmd --get-active-zones 2>/dev/null | tr '\n' ' ')"
    echo "firewalld ports: $(firewall-cmd --list-ports 2>&1); services: $(firewall-cmd --list-services 2>&1)"
  fi
  echo "== libraries pe-app needs that aren't found (none listed: all found)"
  ldd "$here/pe-app" 2>&1 | grep -E "not found|openxr|vulkan" || true
}
report > "$logs/system.txt" 2>&1
cat "$logs/system.txt"

export PE_XR_LOG=1 PE_CUT_LOG="$logs/cuts.txt" PE_TICK_TRACE="$logs/trace.txt"
# A Steam Audio trial kit carries libphonon.so; a build without the trial feature ignores these.
export PE_STEAM_AUDIO_LOG="$logs/steam-audio.txt"
[ -n "${PE_STEAM_AUDIO_LIB:-}" ] || [ ! -e "$here/libphonon.so" ] || export PE_STEAM_AUDIO_LIB="$here/libphonon.so"
# No script toolchain: both machines' build ids must say the same (@engine-architect).
unset CARGO
args=()
case " $* " in *" --headless "*) ;; *) [ "${PE_KIT_NO_VR:-0}" = 1 ] || args+=(--vr) ;; esac
# Steam Audio writes silence unless asked: audible only for the headset run, not for --headless or PE_KIT_NO_VR
# checks (@audio-eval's kit contract).
case " ${args[*]} " in *" --vr "*) export PE_STEAM_AUDIO_AUDIBLE=${PE_STEAM_AUDIO_AUDIBLE:-1} ;; esac
# A session of its own opens the friend test's plane (@coordinator-opus); pe-app's own default is the
# ball pit. Passed with --join too, where pe-app ignores it, so the menu's relaunches (Leave, New session) keep it
# (@design-reviewer's L1 on S6).
case " $* " in *" --preset "*) ;; *) args+=(--preset plane) ;; esac
# pe-app's MCP server, on this machine only (127.0.0.1 and ::1, never the network): an agent here can drive the
# game. Its endpoint and token go to friend-test/mcp.json beside the identity file, not to the logs (the friend
# test 11-27, user).
case " $* " in *" --mcp-http "*|*" --mcp-pipe "*) ;; *) args+=(--mcp-http) ;; esac
echo "== pe-app ${args[*]} $* (logs: $logs)"
"$here/pe-app" "${args[@]}" --log "$logs/pe-app.log" "$@" 2> >(tee "$logs/stderr.txt" >&2)
code=$?
# A menu restart (New session, Join) replaces the process in place, so this line comes once, after the last run.
case $code in
  0) what="" ;;
  # pe-app's EXIT_INTERRUPTED: `quit` or Ctrl-C before there was a session to leave (a joiner still waiting).
  130) what=", stopped by quit or Ctrl-C: not a failure" ;;
  *) what=", an error" ;;
esac
echo "pe-app ended ($code$what). Logs: $logs. Send them with: $here/collect-logs.sh"
exit $code
