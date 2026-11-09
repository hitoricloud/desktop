#!/usr/bin/env bash
# Screenshot the app in a headless sway with software Vulkan: scripts/shot.sh out.png [WxH] [settle-seconds] [-- app args]
set -euo pipefail
out=${1:?out.png}; size=${2:-1440x900}; settle=${3:-4}; shift $(( $# < 3 ? $# : 3 )); [[ ${1:-} == -- ]] && shift
root=$(cd "$(dirname "$0")/.." && pwd)
nb() { nix build --option substituters https://cache.nixos.org --no-link --print-out-paths "nixpkgs#$1" | head -1; }
sway=$(nb sway)/bin/sway; grim=$(nb grim)/bin/grim; mesa=$(nb mesa)
run=$(mktemp -d); trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$run"' EXIT
printf 'xwayland disable\noutput HEADLESS-1 resolution %s scale 1\ndefault_border none\nfor_window [app_id=".*"] floating enable, move position 0 0, border none\nseat * hide_cursor 1\n' "$size" > "$run/sway.conf"
export XDG_RUNTIME_DIR=$run WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
env -u WAYLAND_DISPLAY -u DISPLAY "$sway" -c "$run/sway.conf" >"$run/sway.log" 2>&1 &
for _ in $(seq 50); do sock=$(ls "$run" | grep -m1 '^wayland-[0-9]$' || true); [[ -n $sock ]] && break; sleep 0.1; done
export WAYLAND_DISPLAY=$sock VK_ICD_FILENAMES=$mesa/share/vulkan/icd.d/lvp_icd.x86_64.json
HITORI_SIZE=$size "$root/target/debug/hitori" "$@" >"$run/app.log" 2>&1 &
sleep "$settle"
if [[ -n ${HITORI_FRAMES:-} ]]; then
  # Burst: HITORI_FRAMES=count HITORI_EVERY=seconds → out-00.png, out-01.png, …
  for i in $(seq 0 $((HITORI_FRAMES - 1))); do "$grim" "${out%.png}-$(printf %02d "$i").png"; sleep "${HITORI_EVERY:-0.05}"; done
else
  "$grim" "$out" || { cat "$run/app.log" "$run/sway.log" >&2; exit 1; }
fi
echo "--- app.log"; tail -20 "$run/app.log" >&2; echo "--- sway.log"; grep -iE "error|fail" "$run/sway.log" | tail -5 >&2 || true
