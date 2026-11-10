#!/usr/bin/env bash
# dist/hitori-<version>-linux-x86_64.tar.gz from target/release/hitori and bin/xray-linux-amd64 (scripts/fetch-xray.sh).
# Unpack anywhere: bin/hitori finds the core at ../libexec/hitori/xray.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="${VERSION:?set VERSION}"
name="hitori-$version-linux-x86_64"
stage="$root/dist/stage/$name"

rm -rf "$stage"
install -Dm755 "$root/target/release/hitori" "$stage/bin/hitori"
install -Dm755 "$root/bin/xray-linux-amd64" "$stage/libexec/hitori/xray"
install -Dm644 "$root/packaging/linux/hitori.desktop" "$stage/share/applications/hitori.desktop"
install -Dm644 "$root/packaging/hitori.svg" "$stage/share/icons/hicolor/scalable/apps/hitori.svg"
install -Dm644 "$root/LICENSE" "$stage/share/doc/hitori/LICENSE"
install -Dm644 "$root/NOTICE" "$stage/share/doc/hitori/NOTICE"
install -Dm644 "$root/packaging/hitori.png" "$stage/share/icons/hicolor/1024x1024/apps/hitori.png"

tar -C "$root/dist/stage" --owner=0 --group=0 --sort=name -czf "$root/dist/$name.tar.gz" "$name"
rm -rf "$root/dist/stage"
echo "package-linux: dist/$name.tar.gz"
