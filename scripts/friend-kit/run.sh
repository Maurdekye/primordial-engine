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
  echo "XR_RUNTIME_JSON=${XR_RUNTIME_JSON:-<unset>}"
  local dirs="${XDG_CONFIG_HOME:-$HOME/.config}:${XDG_CONFIG_DIRS:-/etc/xdg}" d f
  local IFS=:
  for d in $dirs; do
    for f in "$d/openxr/1/active_runtime.$(uname -m).json" "$d/openxr/1/active_runtime.json"; do
      [ -e "$f" ] || continue
      echo "found: $f -> $(readlink -f "$f")"
      grep -E '"library_path"|"name"' "$f" | sed 's/^ */  /'
      local lib
      lib=$(sed -n 's/.*"library_path" *: *"\([^"]*\)".*/\1/p' "$f")
      case "$lib" in
        /*) [ -e "$lib" ] && echo "  library: present" || echo "  library: MISSING ($lib)" ;;
        "") ;;
        *) [ -e "$(dirname "$(readlink -f "$f")")/$lib" ] && echo "  library: present (relative)" || echo "  library: relative, not found beside the manifest" ;;
      esac
    done
  done
  unset IFS
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
# A session of its own opens the friend test's plane (@coordinator-opus); pe-app's own default is the
# ball pit. Passed with --join too, where pe-app ignores it, so the menu's relaunches (Leave, New session) keep it
# (@design-reviewer's L1 on S6).
case " $* " in *" --preset "*) ;; *) args+=(--preset plane) ;; esac
echo "== pe-app ${args[*]} $* (logs: $logs)"
"$here/pe-app" "${args[@]}" --log "$logs/pe-app.log" "$@" 2> >(tee "$logs/stderr.txt" >&2)
code=$?
echo "pe-app ended ($code). Logs: $logs. Send them with: $here/collect-logs.sh"
echo "(If it restarted itself from the menu, the new one is still running and writes to the same folder.)"
exit $code
