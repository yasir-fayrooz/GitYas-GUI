#!/usr/bin/env bash
# scripts/release/manifest.sh <version> <asset-base-url> <notes-file> <dist-dir>
# Merges the per-platform fragments from package-*.sh (<dist-dir>/*.update.json) into <dist-dir>/latest.json, the static
# manifest the app's updater fetches. Bundle URLs point at <asset-base-url>/<file>: where the release's files are
# downloaded from (GitHub: https://github.com/<owner>/<repo>/releases/download/v<version>; docs/releasing.md).
set -euo pipefail

version="${1:?version}"
base="${2:?asset-base-url}"
notes="${3:?notes-file}"
dist="${4:?dist-dir}"
base="${base%/}"

shopt -s nullglob
fragments=("$dist"/*.update.json)
if [[ ${#fragments[@]} -eq 0 ]]; then echo "error: no *.update.json in $dist" >&2; exit 1; fi

jq -s \
  --arg version "$version" \
  --arg prefix "$base" \
  --arg date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --rawfile notes "$notes" \
  '{
     version: $version,
     notes: $notes,
     pub_date: $date,
     platforms: (add | with_entries(.value = {
       url: ($prefix + "/" + (.value.file | @uri)),
       signature: .value.signature,
       format: .value.format
     }))
   }' "${fragments[@]}" > "$dist/latest.json"
rm "${fragments[@]}"
jq '.platforms | map_values(.url)' "$dist/latest.json"
