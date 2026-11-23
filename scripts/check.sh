#!/usr/bin/env bash
# Workspace gate (CI runs it on every OS). Run from anywhere: scripts/check.sh
# Set CHECK_SKIP_TESTS=1 to skip `cargo test` (e.g. on machines without a GPU for the egui snapshot tests) and
# CHECK_SKIP_CLIPPY=1 to skip clippy (CI: only the gates that need no compilation).
set -euo pipefail
cd "$(dirname "$0")/.."

BACKEND_CRATES=(gityas-core gityas-git gityas-storage gityas-auth gityas-platform gityas-engine)
fail=0
step() { printf '\n==> %s\n' "$*"; }
bad() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

step "cargo fmt --check"
cargo fmt --all --check || bad "formatting"

if [[ "${CHECK_SKIP_CLIPPY:-0}" != 1 ]]; then
  step "cargo clippy -D warnings"
  cargo clippy --workspace --all-targets -- -D warnings || bad "clippy"
fi

if [[ "${CHECK_SKIP_TESTS:-0}" != 1 ]]; then
  step "cargo test"
  cargo test --workspace || bad "tests"
fi

step "dependency rule"
deps_of() { cargo tree -p "$1" --prefix none --edges normal | awk '{print $1}' | sort -u; }
for pkg in "${BACKEND_CRATES[@]}"; do
  if deps_of "$pkg" | grep -qxE 'egui|eframe'; then bad "$pkg depends on egui/eframe"; fi
done
if deps_of gityas-git | grep -qx rusqlite; then bad "gityas-git depends on rusqlite"; fi
if deps_of gityas-storage | grep -qx git2; then bad "gityas-storage depends on git2"; fi
for pkg in gityas-git gityas-storage gityas-auth gityas-platform; do
  for other in gityas-git gityas-storage gityas-auth gityas-platform gityas-engine; do
    [[ "$other" == "$pkg" ]] && continue
    if deps_of "$pkg" | grep -qx "$other"; then bad "$pkg depends on $other (only gityas-core allowed)"; fi
  done
done
for other in gityas-git gityas-storage gityas-auth gityas-platform gityas-engine; do
  if deps_of gityas-core | grep -qx "$other"; then bad "gityas-core depends on $other"; fi
done

# The frontend may use only engine, core and platform (direct dependencies; the engine brings the rest).
direct_deps() { cargo tree -p "$1" --prefix none --edges normal --depth 1 | awk 'NR>1{print $1}' | sort -u; }
for other in gityas-git gityas-storage gityas-auth; do
  if direct_deps gityas-frontend | grep -qx "$other"; then bad "gityas-frontend depends directly on $other"; fi
done

if [[ $fail -ne 0 ]]; then printf '\ncheck.sh: FAILED\n' >&2; exit 1; fi
printf '\ncheck.sh: all checks passed\n'
