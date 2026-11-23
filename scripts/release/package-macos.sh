#!/usr/bin/env bash
# scripts/release/package-macos.sh <aarch64|x86_64> <version> <out-dir>
# On a Mac (the release workflow's runner) builds natively with cargo and the installed SDK; on any other host
# cross-compiles with cargo-zigbuild, given a macOS SDK in SDKROOT. Assembles GitYas.app, signs it with rcodesign and
# notarizes + staples it when configured. Writes the updater bundle (.app.tar.gz + .sig) and a .zip for first installs.
#   SDKROOT              an extracted MacOSX*.sdk (frameworks to link against; cross builds only)
#   APPLE_P12_PATH       Developer ID Application certificate + key (.p12), with APPLE_P12_PASSWORD
#   APPLE_API_KEY_JSON   App Store Connect API key, `rcodesign encode-app-store-connect-api-key` output (notarization)
# Without a certificate the app is signed ad hoc: valid, but Gatekeeper warns on first launch (docs/releasing.md).
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/release/lib.sh

arch="${1:?arch (aarch64 or x86_64)}"
version="${2:?version}"
out="${3:?out-dir}"
case "$arch" in aarch64 | x86_64) ;; *) echo "error: unknown arch $arch" >&2; exit 1 ;; esac
target="$arch-apple-darwin"
need rcodesign jq
openssl_build_tools "$target"
if [[ "$(uname -s)" == Darwin ]]; then
  rustup target add "$target" > /dev/null
  cargo build --release -p gityas-frontend --target "$target"
else
  need cargo-zigbuild zig
  [[ -d "${SDKROOT:-}/System/Library/Frameworks" ]] || { echo "error: SDKROOT must point at a MacOSX*.sdk (docs/releasing.md)" >&2; exit 1; }
  cargo zigbuild --release -p gityas-frontend --target "$target"
fi

bundle_dir="target/$target/release/bundle"
app="$bundle_dir/GitYas.app"
mkdir -p "$bundle_dir"
pack icns "$bundle_dir/GitYas.icns" src-frontend/assets/{32x32,128x128,128x128@2x,icon}.png
pack macos-app "$app" "target/$target/release/gityas" "$bundle_dir/GitYas.icns" "$version"

if [[ -n "${APPLE_P12_PATH:-}" ]]; then
  password_file=$(mktemp)
  trap 'rm -f "$password_file"' EXIT
  printf '%s' "${APPLE_P12_PASSWORD:-}" > "$password_file"
  # --for-notarization: hardened runtime and a secure timestamp, which notarization requires.
  rcodesign sign --p12-file "$APPLE_P12_PATH" --p12-password-file "$password_file" --for-notarization "$app"
  if [[ -n "${APPLE_API_KEY_JSON:-}" ]]; then
    rcodesign notary-submit --api-key-file "$APPLE_API_KEY_JSON" --staple "$app"
  else
    echo "warning: no APPLE_API_KEY_JSON: signed but not notarized, Gatekeeper still warns" >&2
  fi
else
  # Ad hoc (no certificate) rather than unsigned: an unsigned download is "damaged" to macOS, ad hoc only
  # "could not be verified", which users can pass with Privacy & Security -> Open Anyway.
  echo "warning: no APPLE_P12_PATH: the macOS app is signed ad hoc and not notarized" >&2
  rcodesign sign "$app"
fi
rcodesign verify "$app/Contents/MacOS/gityas"

# The updater unpacks the .app.tar.gz over the installed GitYas.app; the .zip is the first-install download.
tarball="$bundle_dir/GitYas_${version}_${arch}.app.tar.gz"
pack targz "$tarball" "$app"
sign_update "$tarball"
pack zip "$out/GitYas_${version}_${arch}.zip" "$app"
update_fragment "macos-$arch" "$tarball" app "$out"
