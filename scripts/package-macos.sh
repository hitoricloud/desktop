#!/usr/bin/env bash
# dist/hitori-<version>-macos-universal.dmg: a universal Hitori VPN.app (arm64 + x86_64) with the Xray core inside.
# Needs target/{aarch64,x86_64}-apple-darwin/release/hitori and bin/xray-darwin-{arm64,amd64} (scripts/fetch-xray.sh).
# Signs with $MACOS_SIGN_IDENTITY when set, else ad hoc (Gatekeeper then asks to confirm the first launch).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="${VERSION:?set VERSION}"
bundle_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)"
identity="${MACOS_SIGN_IDENTITY:--}"
timestamp=--timestamp
[[ "$identity" != - ]] || timestamp=--timestamp=none
stage="$root/dist/stage"
app="$stage/Hitori VPN.app"

rm -rf "$stage"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create "$root/target/aarch64-apple-darwin/release/hitori" "$root/target/x86_64-apple-darwin/release/hitori" -output "$app/Contents/MacOS/hitori"
lipo -create "$root/bin/xray-darwin-arm64" "$root/bin/xray-darwin-amd64" -output "$app/Contents/MacOS/xray"
chmod 755 "$app/Contents/MacOS/"*

iconset="$stage/hitori.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size "$root/packaging/hitori.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) "$root/packaging/hitori.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/hitori.icns"
rm -rf "$iconset"

cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>Hitori VPN</string>
  <key>CFBundleExecutable</key><string>hitori</string>
  <key>CFBundleIconFile</key><string>hitori</string>
  <key>CFBundleIdentifier</key><string>cloud.hitori.vpn</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Hitori VPN</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$bundle_version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>LSMinimumSystemVersion</key><string>${MACOSX_DEPLOYMENT_TARGET:-11.0}</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# The nested core first, then the bundle that seals it.
codesign --force --options runtime $timestamp --sign "$identity" "$app/Contents/MacOS/xray"
codesign --force --options runtime $timestamp --sign "$identity" "$app"
codesign --verify --strict "$app"

ln -s /Applications "$stage/Applications"
dmg="$root/dist/hitori-$version-macos-universal.dmg"
rm -f "$dmg"
hdiutil create -volname "Hitori VPN" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"
rm -rf "$stage"
echo "package-macos: ${dmg#"$root/"}"
