#!/usr/bin/env bash
# scripts/release/package-linux.sh <version> <out-dir>
# Cross-compiles with cargo-zigbuild (any host) against glibc 2.31 (Ubuntu 20.04, Debian 11 and newer), then builds
# the AppImage (AppImage type 2 runtime + a squashfs from gensquashfs, squashfs-tools-ng) and the .deb, and writes the
# AppImage's update signature (.sig). No Linux machine needed.
#   GENSQUASHFS  path to gensquashfs if it is not on PATH
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/release/lib.sh

version="${1:?version}"
out="${2:?out-dir}"
target=x86_64-unknown-linux-gnu
glibc=2.31
gensquashfs="${GENSQUASHFS:-gensquashfs}"
need cargo-zigbuild zig jq "$gensquashfs"
openssl_build_tools "$target"

# The AppImage runtime (the ELF stub that mounts the appended squashfs), pinned by checksum.
runtime_url=https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64
runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
runtime=target/release-tools/appimage-runtime-x86_64-20251108
if [[ ! -f "$runtime" ]]; then
  mkdir -p "$(dirname "$runtime")"
  curl -fsSL "$runtime_url" -o "$runtime.part"
  mv "$runtime.part" "$runtime"
fi
echo "$runtime_sha256  $runtime" | sha256sum -c --quiet - || { rm -f "$runtime"; echo "error: AppImage runtime checksum mismatch" >&2; exit 1; }

cargo zigbuild --release -p gityas-frontend --target "$target.$glibc"
binary="target/$target/release/gityas"

bundle_dir="target/$target/release/bundle"
mkdir -p "$bundle_dir"
pack deb "$out/gityas_${version}_amd64.deb" "$version" "$binary" src-frontend/assets

# gensquashfs reads the pack file's sources relative to -D (the staged AppDir pack.py prints) and takes every mode,
# owner and the AppRun symlink from the pack file, so nothing depends on the Windows filesystem.
appdir=$(pack squashfs-list "$bundle_dir/appimage.list" "$binary" src-frontend/assets)
"$gensquashfs" -q -f -c gzip -d mode=0755,uid=0,gid=0 -F "$bundle_dir/appimage.list" -D "$appdir" "$bundle_dir/appimage.squashfs"
appimage="$bundle_dir/gityas_${version}_x86_64.AppImage"
cat "$runtime" "$bundle_dir/appimage.squashfs" > "$appimage"
sign_update "$appimage"
update_fragment linux-x86_64 "$appimage" appimage "$out"
