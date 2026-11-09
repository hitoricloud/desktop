#!/usr/bin/env bash
# Screenshot every state in scripts/states.txt from GPUI (headless sway) and the web prototype (needs its dev server),
# then write side-by-side and diff images plus a mismatch percentage. Run inside `nix develop`.
#   scripts/compare.sh [outdir] [web-base-url]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-/tmp/hitori-compare}; base=${2:-http://localhost:5176/}
mkdir -p "$out/web" "$out/gpui" "$out/diff"
magick=$(nix build --option substituters https://cache.nixos.org --no-link --print-out-paths nixpkgs#imagemagick | grep -v -- -dev | head -1)/bin/magick
source "$HOME/.local/share/playwright/env.sh"
env -u WAYLAND_DISPLAY -u DISPLAY node "$root/scripts/web-states.mjs" "$out/web" "$base"
grep -v '^#' "$root/scripts/states.txt" | while IFS='|' read -r name size state _; do
  name=$(echo "$name" | xargs); size=$(echo "$size" | xargs); state=$(echo "$state" | xargs)
  [[ -z $name ]] && continue
  HITORI_STATE=$state "$root/scripts/shot.sh" "$out/gpui/$name.png" "$size" 12 >/dev/null 2>&1
  bad=$("$magick" compare -metric AE -fuzz 9.5% "$out/web/$name.png" "$out/gpui/$name.png" "$out/diff/$name.png" 2>&1 >/dev/null | cut -d' ' -f1 || true)
  total=$(( ${size%x*} * ${size#*x} ))
  "$magick" "$out/web/$name.png" "$out/gpui/$name.png" "$out/diff/$name.png" +append "$out/diff/$name-side.png"
  awk -v n="$name" -v b="${bad:-0}" -v t="$total" 'BEGIN { printf "%-10s %6.2f%% pixels differ\n", n, b * 100 / t }'
done
