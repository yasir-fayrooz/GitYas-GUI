#!/usr/bin/env bash
# scripts/release/publish-github.sh <version>
# Uploads dist/v<version> (scripts/release/release.sh, or the release workflow's publish job) to a **draft** GitHub
# release v<version> on RELEASE_REPO, with that version's CHANGELOG notes. Installed apps only see the update once you
# press Publish on GitHub: the updater reads releases/latest, which skips drafts and pre-releases.
#
# Needs the GitHub CLI (`gh`, signed in, or GH_TOKEN) and tag v<version> pushed to RELEASE_REPO.
# RELEASE_REPO comes from scripts/release/release.env when it exists, else the environment.
#   RELEASE_REUPLOAD=1  replace the files of an existing **draft** v<version> (a published release is never changed:
#                       its signatures are already in installed apps' hands)
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/release/lib.sh

version="${1:?usage: publish-github.sh <version>}"
env_file=scripts/release/release.env
if [[ -z "${RELEASE_REPO:-}" && -f "$env_file" ]]; then
  RELEASE_REPO=$(set -a; source "$env_file"; printf '%s' "${RELEASE_REPO:-}")
fi
[[ -n "${RELEASE_REPO:-}" ]] || { echo "error: RELEASE_REPO is not set (<owner>/<repo>)" >&2; exit 1; }
need gh jq

work="dist/v$version"
notes="dist/release-notes-$version.md"
[[ -f "$work/latest.json" ]] || { echo "error: $work/latest.json not found (run scripts/release/release.sh $version)" >&2; exit 1; }
[[ -f "$notes" ]] || bash scripts/release/prepare.sh "$version" "$notes"
[[ "$(jq -r .version "$work/latest.json")" == "$version" ]] || { echo "error: $work/latest.json is not version $version" >&2; exit 1; }

shopt -s nullglob
files=("$work"/*)

# Every bundle the manifest names must be one of the files being uploaded, at this release's URL (manifest.sh
# writes <asset base>/<file name, URI-encoded>).
unlisted=$(printf '%s\n' "${files[@]##*/}" | jq -R . | jq -rs --arg base "$(release_asset_base "$version")/" \
  --slurpfile manifest "$work/latest.json" \
  '(map($base + @uri) ) as $urls | [$manifest[0].platforms[].url | select(. as $u | $urls | index($u) | not)] | .[]')
if [[ -n "$unlisted" ]]; then
  while read -r url; do
    echo "error: latest.json points at $url, which is not a file in $work (RELEASE_REPO or version changed?)" >&2
  done <<< "$unlisted"
  exit 1
fi
tag="v$version"
if existing=$(gh release view "$tag" --repo "$RELEASE_REPO" --json isDraft --jq .isDraft 2> /dev/null); then
  if [[ "$existing" != true ]]; then
    echo "error: $tag is already published on $RELEASE_REPO. Release a new version instead." >&2
    exit 1
  fi
  [[ -n "${RELEASE_REUPLOAD:-}" ]] || { echo "error: draft $tag already exists (RELEASE_REUPLOAD=1 replaces its files)" >&2; exit 1; }
  gh release upload "$tag" "${files[@]}" --repo "$RELEASE_REPO" --clobber
else
  gh release create "$tag" "${files[@]}" --repo "$RELEASE_REPO" --draft --verify-tag \
    --title "GitYas $version" --notes-file "$notes"
fi
echo "Draft $tag is on https://github.com/$RELEASE_REPO/releases: check it, then press Publish."
