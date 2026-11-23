#!/usr/bin/env bash
# Release gate: scripts/release/prepare.sh <version> <notes-out>
# Fails unless <version> is the workspace version and CHANGELOG.md has a `## [<version>]` section with notes, then
# writes that section's notes to <notes-out> (the manifest's `notes`).
set -euo pipefail
cd "$(dirname "$0")/../.."

version="${1:?usage: prepare.sh <version> <notes-out>}"
out="${2:?usage: prepare.sh <version> <notes-out>}"

workspace=$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "gityas-frontend") | .version')
if [[ "$workspace" != "$version" ]]; then
  echo "error: tag is v$version but [workspace.package] version in Cargo.toml is $workspace" >&2
  exit 1
fi

# Lines after `## [X.Y.Z]` / `## X.Y.Z` / `## vX.Y.Z` up to the next `## ` heading.
awk -v v="$version" '
  /^## / {
    if (found) exit
    h = $2; gsub(/^\[|\]$/, "", h); sub(/^[vV]/, "", h)
    if (h == v) { found = 1; next }
  }
  found { print }
' CHANGELOG.md | sed -e '/./,$!d' > "$out"

if ! grep -q '[^[:space:]]' "$out"; then
  echo "error: CHANGELOG.md has no notes for $version (add a '## [$version] - YYYY-MM-DD' section)" >&2
  exit 1
fi
echo "Release $version, notes:"
cat "$out"
