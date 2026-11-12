#!/usr/bin/env bash
# Installs the Xray core from a release of the public fork (github.com/hitoricloud/Xray-core) into bin/.
# Each release asset is checked against the SHA-256 in its .dgst file.
#
#   scripts/fetch-xray.sh [tag]        the pinned release (XRAY_TAG) -> bin/xray
#   XRAY_TARGETS="linux-amd64 windows-amd64 darwin-arm64 darwin-amd64" scripts/fetch-xray.sh
#                                      several cores -> bin/xray-<target>[.exe] (CI packaging)
# windows-amd64 also fetches the signed wintun.dll (TUN mode) from wintun.net -> bin/wintun.dll, checked against WINTUN_SHA256.
set -euo pipefail

repo="${XRAY_REPO:-hitoricloud/Xray-core}"
tag="${1:-${XRAY_TAG:-v26.11.9-hitori.1}}"
wintun_version=0.14.1
wintun_sha256=07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51
root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/bin"

# Upstream's release names (.github/build/friendly-filenames.json in Xray-core).
asset() {
  case "$1" in
    linux-amd64) echo linux-64 ;;
    windows-amd64) echo windows-64 ;;
    darwin-amd64) echo macos-64 ;;
    darwin-arm64) echo macos-arm64-v8a ;;
    *) echo "fetch-xray: unknown target $1" >&2; return 1 ;;
  esac
}

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$out"
cores='{}'
linux=""
for target in ${XRAY_TARGETS:-linux-amd64}; do
  zip="Xray-$(asset "$target").zip"
  url="https://github.com/$repo/releases/download/$tag/$zip"
  curl -fsSL --retry 2 -o "$tmp/$zip" "$url"
  curl -fsSL --retry 2 -o "$tmp/$zip.dgst" "$url.dgst"
  # openssl dgst lines: "SHA2-256= <hex>" (OpenSSL 3) or "SHA256= <hex>".
  want="$(sed -n 's/^SHA2\{0,1\}-\{0,1\}256= *//p' "$tmp/$zip.dgst" | head -n1)"
  got="$(sha256 "$tmp/$zip")"
  [[ -n "$want" && "$want" == "$got" ]] || { echo "fetch-xray: $zip checksum mismatch (want '$want', got '$got')" >&2; exit 1; }

  ext=""
  [[ "$target" == windows-* ]] && ext=".exe"
  mkdir "$tmp/$target"
  unzip -q -o "$tmp/$zip" "xray$ext" -d "$tmp/$target"
  dest="$out/xray-$target$ext"
  [[ -n "${XRAY_TARGETS:-}" ]] || dest="$out/xray"
  install -m 755 "$tmp/$target/xray$ext" "$dest"
  [[ "$target" == linux-amd64 ]] && linux="$dest"
  cores="$(jq --arg t "$target" --arg sha256 "$got" '. + {($t): {zip: $sha256}}' <<<"$cores")"
  if [[ "$target" == windows-amd64 ]]; then
    # Wintun ships only as this signed DLL; it must sit next to xray.exe.
    curl -fsSL --retry 3 -o "$tmp/wintun.zip" "https://www.wintun.net/builds/wintun-$wintun_version.zip"
    got="$(sha256 "$tmp/wintun.zip")"
    [[ "$got" == "$wintun_sha256" ]] || { echo "fetch-xray: wintun-$wintun_version.zip checksum mismatch (got '$got')" >&2; exit 1; }
    unzip -q -o "$tmp/wintun.zip" wintun/bin/amd64/wintun.dll -d "$tmp"
    install -m 644 "$tmp/wintun/bin/amd64/wintun.dll" "$out/wintun.dll"
    cores="$(jq --arg v "$wintun_version" --arg sha256 "$got" '. + {wintun: {version: $v, zip: $sha256}}' <<<"$cores")"
  fi
done

version=""
[[ -n "$linux" && "$(uname -sm)" == "Linux x86_64" ]] && version="$("$linux" version | head -n1)"
jq -n --arg repo "$repo" --arg tag "$tag" --arg version "$version" --argjson sha256 "$cores" --arg fetched "$(date -u +%FT%TZ)" \
  '{repo: $repo, tag: $tag, version: $version, sha256: $sha256, fetched: $fetched}' >"$out/xray.json"
echo "fetch-xray: ${version:-Xray} ($repo $tag) -> bin/ [${XRAY_TARGETS:-linux-amd64}]"
