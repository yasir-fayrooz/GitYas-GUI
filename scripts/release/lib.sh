#!/usr/bin/env bash
# Shared by the scripts/release/package-*.sh scripts (sourced, not run). They run from the workspace root (git-yas/)
# in Git Bash on Windows, or bash on macOS / Linux; release.sh exports the configuration they read.

on_windows() { [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* ]]; }

# python3 on macOS / Linux; on Windows `python3` may be the Microsoft Store stub, so prefer `python` there.
if on_windows; then PYTHON=${PYTHON:-python}; else PYTHON=${PYTHON:-python3}; fi

pack() { "$PYTHON" -I scripts/release/pack.py "$@"; }

# The macOS and Linux builds compile OpenSSL from source (git2's vendored-openssl, for libssh2), which runs Perl's
# ./Configure and `make`. Git for Windows' Perl lacks modules Configure needs and has no make, so on Windows use
# MSYS2's (pacman -S perl make). Its usr/bin goes last on PATH: only `make` comes from it, the rest stays Git Bash's.
# OpenSSL's Makefile also runs ranlib through sh, which mangles the backslashes in cargo-zigbuild's Windows wrapper
# path, so name `zig ranlib` instead (cargo-zigbuild keeps a RANLIB_<target> that is already set).
# openssl_build_tools <rust-target>
openssl_build_tools() {
  if on_windows; then
    local target_var="RANLIB_${1//-/_}"
    export "$target_var=zig ranlib"
    local msys2="${MSYS2_ROOT:-/c/msys64}"
    if [[ ! -x "$msys2/usr/bin/perl.exe" || ! -x "$msys2/usr/bin/make.exe" ]]; then
      echo "error: building OpenSSL needs MSYS2's perl and make in $msys2 (docs/releasing.md, Build machine)" >&2
      exit 1
    fi
    OPENSSL_SRC_PERL="$(cygpath -m "$msys2/usr/bin/perl.exe")"
    export OPENSSL_SRC_PERL PATH="$PATH:$msys2/usr/bin"
  else
    need perl make
  fi
}

# Where releases live: GitHub Releases on RELEASE_REPO (<owner>/<repo>). The app's updater fetches the manifest of the
# latest published release (drafts and pre-releases are skipped), and each manifest points at its own release's files.
update_endpoint() { printf 'https://github.com/%s/releases/latest/download/latest.json' "${RELEASE_REPO:?}"; }
# release_asset_base <version>
release_asset_base() { printf 'https://github.com/%s/releases/download/v%s' "${RELEASE_REPO:?}" "$1"; }

need() {
  local tool
  for tool in "$@"; do
    command -v "$tool" > /dev/null || { echo "error: $tool is not installed (docs/releasing.md, Build machine)" >&2; exit 1; }
  done
}

# The minisign signature the updater verifies (<file>.sig), with the update key from release.env.
sign_update() {
  [[ -n "${CARGO_PACKAGER_SIGN_PRIVATE_KEY:-}" ]] || { echo "error: no update signing key (UPDATE_PRIVATE_KEY_PATH)" >&2; exit 1; }
  rm -f "$1.sig"
  cargo packager signer sign -q "$1"
  [[ -s "$1.sig" ]] || { echo "error: signing $1 produced no .sig" >&2; exit 1; }
}

# The key id (16 hex digits) of a minisign public key or signature, given as cargo-packager writes them: the whole
# minisign file in base64 (update.key.pub, <file>.sig). Fails on anything else.
# minisign_key_id <base64>
minisign_key_id() {
  local key id
  key=$(printf '%s' "$1" | base64 -d 2> /dev/null | sed -n 2p) || return 1
  id=$(printf '%s' "$key" | base64 -d 2> /dev/null | od -An -tx1 -j2 -N8 2> /dev/null | tr -d ' \n') || return 1
  [[ ${#id} -eq 16 ]] || return 1
  printf '%s' "$id"
}

# Copies the updater bundle + its .sig to <out-dir> and writes <out-dir>/<platform>.update.json for manifest.sh:
# update_fragment <platform> <bundle> <format> <out-dir>
update_fragment() {
  local platform="$1" bundle="$2" format="$3" out="$4" name sig_id
  # The app only installs bundles signed by the key compiled into it (UPDATE_PUBKEY), so a mismatch is fatal.
  sig_id=$(minisign_key_id "$(cat "$bundle.sig")") || { echo "error: $bundle.sig is not a minisign signature" >&2; exit 1; }
  if [[ "$sig_id" != "${UPDATE_KEY_ID:?}" ]]; then
    echo "error: $bundle is signed with key $sig_id, but UPDATE_PUBKEY is key $UPDATE_KEY_ID" >&2
    exit 1
  fi
  name=$(basename "$bundle")
  cp "$bundle" "$out/$name"
  cp "$bundle.sig" "$out/$name.sig"
  jq -n --arg p "$platform" --arg file "$name" --arg sig "$(cat "$bundle.sig")" --arg format "$format" \
    '{($p): {file: $file, signature: $sig, format: $format}}' > "$out/$platform.update.json"
}
