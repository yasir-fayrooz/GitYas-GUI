# Developing GitYas

How the code is laid out, and how to build, test and package it. For what GitYas does, see the [README](../README.md).

## Layout

```
src-frontend/            egui app (binary `gityas`), icons in assets/
src-backend/
  gityas-core/           shared types: models, errors, events, diff types
  gityas-git/            git operations, merge engine, structured diffs
  gityas-storage/        SQLite user DB and disposable cache DB
  gityas-auth/           personal access tokens in the OS keychain
  gityas-platform/       data / cache / log directories, logging, opening URLs
  gityas-engine/         application layer the UI talks to
docs/                    release guide
scripts/check.sh         the CI gates
scripts/release/         release builds (local and GitHub Actions)
```

Dependency rule: `core ← git, storage, auth, platform ← engine ← frontend`.

## Build and run

Rust 1.95 (pinned in `rust-toolchain.toml`). On Linux, install `libdbus-1-dev`, `libxkbcommon-dev`,
`libssl-dev` and `pkg-config` first.

```bash
cargo run -p gityas-frontend
```

`-- --mock` runs against built-in mock data with an in-memory engine (no keychain, no data directory).

**macOS keychain prompts.** Cargo signs builds ad hoc, so after every rebuild the keychain treats GitYas as a new app
and asks for your password once per saved token. Run `scripts/macos-dev-identity.sh` once: it creates a local
self-signed "GitYas Dev" code-signing certificate, and `cargo run` (through the runner in `.cargo/config.toml`) then
signs every build with it. Answer "Always Allow" once per item and the prompts stop. (Keychain Access → Certificate
Assistant → Create a Certificate…, name "GitYas Dev", type Code Signing, works too.) Released builds need the Developer
ID signing in `releasing.md` §5 for the same reason.

## Test

```bash
cargo test --workspace
```

The egui snapshot tests need a GPU and are ignored by default:

```bash
cargo test -p gityas-frontend -- --ignored
```

Set `UPDATE_SNAPSHOTS=1` to regenerate the reference images in `src-frontend/tests/snapshots/`.

`scripts/check.sh` runs formatting, clippy, the tests and the dependency rule; CI runs it on Windows, macOS and
Linux. `CHECK_SKIP_TESTS=1` skips the tests.

## Package

With [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) (`cargo install cargo-packager --locked`):

```bash
cargo packager --release -p gityas-frontend -f nsis
```

Formats: `nsis` (setup .exe) on Windows, `app,dmg` on macOS, `deb,appimage` on Linux. Releases are built by GitHub
Actions when a `v*` tag is pushed and published as GitHub Releases, which the app's auto-updater reads. The same
scripts work locally: `scripts/release/release.sh <version>` builds Windows natively, cross-compiles macOS and Linux
with cargo-zigbuild, code-signs (rcodesign for macOS) and packages all of them into `dist/v<version>/` with the
auto-updater manifest, and `scripts/release/publish-github.sh <version>` uploads them to a draft release. Release
notes live in [CHANGELOG.md](../CHANGELOG.md) and appear in the app after an update. Setup and the release checklist:
[releasing.md](releasing.md).

## Screenshots and animations

The images in the README come from the egui snapshot tests in `src-frontend/tests/snapshots/` (regenerate them with
`UPDATE_SNAPSHOTS=1 cargo test -p gityas-frontend -- --ignored`). The animations in `docs/images/` are built from
those snapshots by `bun run demos` in the website (`gityas-website/scripts/demos.mjs` in the GitYas repository).
