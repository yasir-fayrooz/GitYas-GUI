#!/usr/bin/env bash
# scripts/release/package-windows.sh <version> <out-dir>
# Native build on Windows (MSVC): cargo-packager builds the NSIS installer, signs gityas.exe and the installer with
# Authenticode when configured, and writes the update signature (.sig).
#   WINDOWS_CERTIFICATE_THUMBPRINT  a code-signing certificate in the CurrentUser\My store (signtool from the
#                                   Windows SDK), with WINDOWS_TIMESTAMP_URL (default DigiCert's)
#   WINDOWS_SIGN_COMMAND            or a command that signs the file passed as %1 (Azure Artifact Signing, a cloud HSM)
# Neither set: the installer is unsigned (SmartScreen warns on first install; updates are still verified).
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/release/lib.sh

version="${1:?version}"
out="${2:?out-dir}"
target=x86_64-pc-windows-msvc
on_windows || { echo "error: the Windows installer is built on Windows" >&2; exit 1; }
need cargo-packager jq

# cargo-packager reads code-signing settings from Cargo.toml only: add them for this build, then put the file back.
manifest=src-frontend/Cargo.toml
cp "$manifest" "$manifest.release-backup"
trap 'mv -f "$manifest.release-backup" "$manifest"' EXIT
if [[ -n "${WINDOWS_CERTIFICATE_THUMBPRINT:-}" ]]; then
  printf '\n[package.metadata.packager.windows]\ncertificate-thumbprint = "%s"\ndigest-algorithm = "sha256"\ntimestamp-url = "%s"\n' \
    "$WINDOWS_CERTIFICATE_THUMBPRINT" "${WINDOWS_TIMESTAMP_URL:-http://timestamp.digicert.com}" >> "$manifest"
elif [[ -n "${WINDOWS_SIGN_COMMAND:-}" ]]; then
  # TOML literal string: no escaping, so Windows paths work as typed.
  printf "\n[package.metadata.packager.windows]\nsign-command = '%s'\n" "$WINDOWS_SIGN_COMMAND" >> "$manifest"
else
  echo "warning: no Windows code signing configured: the installer is unsigned" >&2
fi

# `cargo build` (the before-packaging-command) builds for this target.
CARGO_BUILD_TARGET=$target cargo packager --release -p gityas-frontend -f nsis --target $target

release_dir="target/$target/release"
# Match this version only: installers from earlier builds stay in the release dir.
mapfile -t found < <(find "$release_dir" -maxdepth 2 -type f -name "*_${version}_*-setup.exe" -not -path '*/.cargo-packager/*')
[[ ${#found[@]} -eq 1 ]] || { echo "error: expected one *_${version}_*-setup.exe in $release_dir, found ${#found[@]}" >&2; exit 1; }
installer="${found[0]}"
[[ -s "$installer.sig" ]] || sign_update "$installer"
update_fragment windows-x86_64 "$installer" nsis "$out"
