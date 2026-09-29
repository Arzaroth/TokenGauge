#!/usr/bin/env bash
# After a release build of both macOS targets, on macOS with librsvg:
#
# - signs the per-architecture binaries in place, which the tarballs ship
# - builds TokenGauge.app, universal, from those binaries
# - puts the app in dist/tokengauge-<tag>-macos-universal.dmg
#
# Signs with MACOS_SIGN_IDENTITY (a Developer ID Application identity in the
# keychain, hardened runtime) when it is set, ad-hoc otherwise. PROFILE picks
# the build to package, release unless CI is checking a debug one.
#   scripts/package-macos.sh <tag>
set -euo pipefail
cd "$(dirname "$0")/.."

tag=$1
safe_tag=${tag//[^A-Za-z0-9._-]/_}
# CFBundleShortVersionString is numeric only.
version=${tag#v}
version=${version%%[-+]*}
if ! [[ $version =~ ^[0-9]+(\.[0-9]+){0,2}$ ]]; then
  echo "package-macos.sh: no numeric version in tag '$tag'" >&2
  exit 1
fi
targets=(aarch64-apple-darwin x86_64-apple-darwin)
binaries=(tokengauge tokengauge-tui tokengauge-tray)
profile=${PROFILE:-release}

identity=${MACOS_SIGN_IDENTITY:--}
sign=(codesign --force --sign "$identity")
if [ "$identity" != - ]; then sign+=(--options runtime --timestamp); fi

for target in "${targets[@]}"; do
  for bin in "${binaries[@]}"; do
    "${sign[@]}" "target/$target/$profile/$bin"
  done
done

app=dist/TokenGauge.app
contents=$app/Contents
rm -rf "$app" dist/dmg
mkdir -p "$contents/MacOS" "$contents/Resources"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$contents/Info.plist"
for bin in "${binaries[@]}"; do
  lipo -create -output "$contents/MacOS/$bin" \
    "target/aarch64-apple-darwin/$profile/$bin" "target/x86_64-apple-darwin/$profile/$bin"
done

# Apple's grid keeps the rounded square at 824 of 1024 points, centred. The
# artwork fills its own canvas, so it is scaled into that box rather than
# stretched to the edge, where it would read larger than every other app.
iconset=dist/TokenGauge.iconset
rm -rf "$iconset" && mkdir -p "$iconset"
render() {
  local size=$1 out=$2 body margin
  body=$((size * 824 / 1024))
  margin=$(((size - body) / 2))
  rsvg-convert -w "$body" -h "$body" --page-width "$size" --page-height "$size" \
    --left "$margin" --top "$margin" assets/tokengauge.svg -o "$out"
}
for size in 16 32 128 256 512; do
  render "$size" "$iconset/icon_${size}x${size}.png"
  render $((size * 2)) "$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$iconset" -o "$contents/Resources/TokenGauge.icns"
rm -rf "$iconset"

# The helpers first: signing the bundle signs its main executable, not the
# other programs beside it.
for bin in tokengauge tokengauge-tui; do "${sign[@]}" "$contents/MacOS/$bin"; done
"${sign[@]}" "$app"
codesign --verify --deep --strict "$app"

dmg="dist/tokengauge-$safe_tag-macos-universal.dmg"
mkdir -p dist/dmg
cp -R "$app" dist/dmg/
ln -s /Applications dist/dmg/Applications
# hdiutil sizes the image from its own estimate of the folder, which comes up
# short often enough to fail with "No space left on device".
size_mb=$(($(du -sm dist/dmg | cut -f1) * 5 / 4 + 20))
rm -f "$dmg"
hdiutil create -volname TokenGauge -srcfolder dist/dmg -size "${size_mb}m" -format UDZO "$dmg"
rm -rf dist/dmg
if [ "$identity" != - ]; then codesign --force --sign "$identity" --timestamp "$dmg"; fi
echo "$dmg"
