#!/usr/bin/env bash
# scripts/release/release.sh [--no-manifest] <version> [windows] [macos-aarch64] [macos-x86_64] [linux]
# Builds, signs and packages GitYas for every platform (or the ones named) and writes the updater manifest
# latest.json, all into dist/v<version>. Windows packages need a Windows host; macOS and Linux are cross-compiled with
# cargo-zigbuild from any host (natively on a Mac). Publishing is a separate step (scripts/release/publish-github.sh).
# Setup and the checklist: docs/releasing.md.
#
# Configuration: scripts/release/release.env when it exists (copy release.env.example), otherwise the environment
# (the release workflow, .github/workflows/release.yml, sets the same variables).
#   --no-manifest          leave the per-platform *.update.json fragments for a later manifest.sh run (one CI job
#                          per platform; the publish job merges them)
#   RELEASE_ALLOW_DIRTY=1  build from uncommitted changes or a commit other than the tag (test builds)
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/release/lib.sh

manifest=1
if [[ "${1:-}" == --no-manifest ]]; then
  manifest=0
  shift
fi
version="${1:?usage: release.sh [--no-manifest] <version> [windows] [macos-aarch64] [macos-x86_64] [linux]}"
shift
platforms=("$@")
[[ ${#platforms[@]} -gt 0 ]] || platforms=(windows macos-aarch64 macos-x86_64 linux)

env_file=scripts/release/release.env
config="the environment"
if [[ -f "$env_file" ]]; then
  set -a
  source "$env_file"
  set +a
  config="$env_file"
fi

# ---- Checks before a long build.
missing=()
for var in RELEASE_REPO UPDATE_PUBKEY UPDATE_PRIVATE_KEY_PATH; do
  [[ -n "${!var:-}" ]] || missing+=("$var")
done
if (( ${#missing[@]} )); then
  for var in "${missing[@]}"; do echo "error: $var is not set in $config" >&2; done
  [[ -f "$env_file" ]] || echo "(no $env_file: copy release.env.example)" >&2
  exit 1
fi
[[ "$RELEASE_REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] \
  || { echo "error: RELEASE_REPO must be <owner>/<repo>, not $RELEASE_REPO" >&2; exit 1; }
[[ -f "$UPDATE_PRIVATE_KEY_PATH" ]] || { echo "error: UPDATE_PRIVATE_KEY_PATH: no file $UPDATE_PRIVATE_KEY_PATH" >&2; exit 1; }
# Compiled in as the updater's key; anything else builds an app that can never update.
UPDATE_KEY_ID=$(minisign_key_id "$UPDATE_PUBKEY") \
  || { echo "error: UPDATE_PUBKEY is not a minisign public key: set it to the contents of update.key.pub" >&2; exit 1; }
export UPDATE_KEY_ID
need cargo jq
if [[ -z "${RELEASE_ALLOW_DIRTY:-}" ]]; then
  [[ -z "$(git status --porcelain)" ]] || { echo "error: uncommitted changes (RELEASE_ALLOW_DIRTY=1 to build anyway)" >&2; exit 1; }
  [[ "$(git rev-parse HEAD)" == "$(git rev-parse -q --verify "v$version^{commit}" || true)" ]] \
    || { echo "error: HEAD is not tag v$version (tag it first, or RELEASE_ALLOW_DIRTY=1)" >&2; exit 1; }
fi

out="dist/v$version"
notes="dist/release-notes-$version.md"
rm -rf "$out"
mkdir -p "$out"
bash scripts/release/prepare.sh "$version" "$notes"

# ---- Compiled into the app: the updater (src-frontend/src/updater.rs).
GITYAS_UPDATE_ENDPOINT="$(update_endpoint)"
export GITYAS_UPDATE_ENDPOINT
export GITYAS_UPDATE_PUBKEY="$UPDATE_PUBKEY"
# cargo-packager / `cargo packager signer sign` sign each updater bundle with these (minisign .sig files).
CARGO_PACKAGER_SIGN_PRIVATE_KEY="$(cat "$UPDATE_PRIVATE_KEY_PATH")"
export CARGO_PACKAGER_SIGN_PRIVATE_KEY
export CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD="${UPDATE_PRIVATE_KEY_PASSWORD:-}"

for platform in "${platforms[@]}"; do
  echo "==> $platform"
  case "$platform" in
    windows) bash scripts/release/package-windows.sh "$version" "$out" ;;
    macos-aarch64 | macos-x86_64) bash scripts/release/package-macos.sh "${platform#macos-}" "$version" "$out" ;;
    linux) bash scripts/release/package-linux.sh "$version" "$out" ;;
    *) echo "error: unknown platform $platform (windows, macos-aarch64, macos-x86_64, linux)" >&2; exit 1 ;;
  esac
done

if (( manifest )); then
  bash scripts/release/manifest.sh "$version" "$(release_asset_base "$version")" "$notes" "$out"
fi
ls -l "$out"
if (( manifest )); then
  echo "Built into $out. Smoke-test it, push tag v$version, then: scripts/release/publish-github.sh $version"
else
  echo "Built into $out (no manifest: run scripts/release/manifest.sh once every platform is in)."
fi
