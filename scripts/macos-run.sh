#!/usr/bin/env bash
# Cargo runner on macOS (see .cargo/config.toml): signs the `gityas` app binary with the local "GitYas Dev" identity
# (created by scripts/macos-dev-identity.sh) before running it, so the keychain sees every rebuild as the same app
# and stops asking for your password. Other binaries (tests, tools) and machines without the identity run as built.
set -euo pipefail

NAME="GitYas Dev"
bin="$1"

if [[ "$(basename "$bin")" == "gityas" ]] && security find-identity -v -p codesigning 2>/dev/null | grep -q "\"$NAME\""; then
  codesign --force --sign "$NAME" --identifier com.gityas.GitYas "$bin" 2>/dev/null \
    || echo "warning: could not sign $bin as \"$NAME\"; the keychain may ask for your password" >&2
fi
exec "$@"
