# Releasing GitYas

How a release gets from a tag to users' machines, the one-time setup it needs, and the checklist for each release.
Releases are built by GitHub Actions ([release.yml](../.github/workflows/release.yml)) and published as GitHub
Releases, which are also where installed apps look for updates. The same scripts build a release on your own machine:
from Windows, macOS and Linux are cross-compiled with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild),
and the macOS app is signed and notarized with [rcodesign](https://gregoryszorc.com/docs/apple-codesign/main/), so a
local release needs no Mac.

## How it fits together

```
git push origin v1.2.0 ──► .github/workflows/release.yml
                             build (windows-latest):  release.sh --no-manifest 1.2.0 windows
                             build (macos-latest):    release.sh --no-manifest 1.2.0 macos-aarch64 macos-x86_64
                             build (ubuntu-22.04):    release.sh --no-manifest 1.2.0 linux
                                  prepare.sh:  tag == Cargo.toml version, CHANGELOG.md has a [1.2.0] section
                                  windows:     cargo packager -f nsis (MSVC) → Authenticode → .sig
                                  macos-*:     cargo build (zigbuild off a Mac) → GitYas.app (pack.py)
                                               → rcodesign sign + notarize + staple
                                               → .app.tar.gz (+ .sig) for the updater, .zip for downloads
                                  linux:       cargo zigbuild (glibc 2.31) → .deb (pack.py)
                                               → AppImage (gensquashfs + AppImage runtime) → .sig
                             publish:  manifest.sh → latest.json; publish-github.sh → draft release v1.2.0
                                   │
          you ─────────────────────┘  check the draft, press Publish
                                   ▼
          GitHub Releases  https://github.com/<RELEASE_REPO>/releases/latest/download/latest.json
                           https://github.com/<RELEASE_REPO>/releases/download/v1.2.0/GitYas_1.2.0_x64-setup.exe (+ .sig, …)
                                   ▲
          GitYas at startup ───────┘  splash: check → download → verify signature → install → relaunch
```

`releases/latest` is the newest **published** release: drafts and pre-releases are skipped, so nothing reaches users
until you press Publish, and a pre-release is a safe place for a test build.

| Script (`scripts/release/`) | Does |
|---|---|
| `release.sh [--no-manifest] <version> [platform…]` | Builds platforms into `dist/v<version>/`. Platforms: `windows`, `macos-aarch64`, `macos-x86_64`, `linux` (default: all). `--no-manifest` leaves the per-platform fragments for a later `manifest.sh` (one CI job per OS). |
| `package-windows.sh`, `package-macos.sh`, `package-linux.sh` | One platform each, into `dist/v<version>/`. Called by `release.sh`, which exports their configuration. |
| `pack.py` | The archives Windows tools cannot write with Unix permissions: `.icns`, `GitYas.app`, `.app.tar.gz`, `.zip`, `.deb`, the AppImage's squashfs layout. |
| `prepare.sh`, `manifest.sh` | Release gate + notes; `latest.json`. |
| `publish-github.sh <version>` | Uploads `dist/v<version>/` to a draft GitHub release `v<version>` (GitHub CLI). |

**In the app** ([updater.rs](../src-frontend/src/updater.rs), [splash.rs](../src-frontend/src/ui/splash.rs)):
release builds show a splash at startup that fetches `latest.json` (6 s timeout). If the version there is newer, it
downloads this platform's bundle with a progress bar, checks its minisign signature against the public key compiled
into the app, installs it and restarts:

| Platform | Bundle | Install |
|---|---|---|
| Windows | NSIS `-setup.exe` | Per-user install (no admin prompt). Runs silently (no installer window), then relaunches GitYas. |
| macOS | `.app.tar.gz` | Replaces the `.app` bundle in place (asks for an admin password only if it is not writable), relaunches. |
| Linux AppImage | `.AppImage` | Replaces the AppImage file in place, relaunches. |
| Linux .deb | — | Cannot replace itself: shows a "GitYas X is available" toast; users update through the new .deb. |

Offline, a server error or a bad signature never blocks the app: the failure is logged and GitYas starts. After 8 s
the splash offers *Continue without updating*. The engine (repositories, watchers) starts only after the splash, so an
install never interrupts git work. Builds without the updater configuration (`cargo run`, tests, local packages)
have no splash at all.

The updater uses [cargo-packager-updater](https://docs.rs/cargo-packager-updater): the same team's companion to
the `cargo packager` tool that builds the Windows installer and signs the bundles, and the same manifest format as
Tauri's updater. GitHub answers the `releases/latest/download/…` and `releases/download/…` URLs with a redirect to
its file storage, which the updater follows.

**What first installs get:** Windows the NSIS installer; macOS a `.zip` of the signed, notarized and stapled
`GitYas.app` (users unzip and drag it to Applications); Linux the AppImage or the `.deb`. All of them are on the
release page.

## One-time setup

### 1. GitHub repository (CI releases)

In the repository's **Settings → Secrets and variables → Actions**:

| Kind | Name | Value |
|---|---|---|
| Variable | `UPDATE_PUBKEY` | contents of `update.key.pub` (§3) |
| Secret | `UPDATE_PRIVATE_KEY` | contents of `update.key` (§3) |
| Secret | `UPDATE_PRIVATE_KEY_PASSWORD` | its password |
| Secret | `APPLE_P12_BASE64` | optional, §5: `base64 -w0 developer-id.p12` (macOS: `base64 -i developer-id.p12`) |
| Secret | `APPLE_P12_PASSWORD` | optional, §5 |
| Secret | `APPLE_API_KEY_JSON` | optional, §5: contents of `app-store-connect.json` |

`RELEASE_REPO` is the repository the workflow runs in. The workflow writes the keys to the runner's temp directory
for the build and deletes them afterwards. Windows code signing is not wired into CI yet (§4): CI installers are
unsigned until it is.

### 2. Local build machine (optional)

Only for building releases on your own machine. Run everything from **Git Bash** in the repository root. Install:

| Tool | Install | Used for |
|---|---|---|
| Rust (rustup) | [rustup.rs](https://rustup.rs); the toolchain comes from `rust-toolchain.toml` | everything |
| Visual Studio Build Tools | "Desktop development with C++" workload (MSVC + Windows SDK, incl. signtool) | Windows build and signing |
| Rust targets | `rustup target add x86_64-unknown-linux-gnu aarch64-apple-darwin x86_64-apple-darwin` | cross builds |
| zig 0.15 | `winget install -e --id zig.zig --version 0.15.2`, then open a new shell | cargo-zigbuild's C compiler and linker |
| cargo-zigbuild | `cargo install --locked cargo-zigbuild@0.24.0` | macOS and Linux builds |
| cargo-packager | `cargo install --locked cargo-packager@0.11.8` | Windows installer, update signatures |
| rcodesign | `cargo install --locked apple-codesign@0.29.0` (or the Windows zip from its [releases](https://github.com/indygreg/apple-platform-rs/releases)) | macOS signing, notarization |
| MSYS2 + perl, make | `winget install -e --id MSYS2.MSYS2`, then in the MSYS2 shell `pacman -S --needed perl make` | compiling OpenSSL for macOS / Linux (below) |
| gensquashfs | squashfs-tools-ng's Windows build, [infraroot.at/pub/squashfs/windows](https://infraroot.at/pub/squashfs/windows/) (`squashfs-tools-ng-1.3.2-mingw64.zip`); put its `bin/` on PATH or set `GENSQUASHFS` | the AppImage |
| jq, Python 3, GitHub CLI | `winget install jqlang.jq Python.Python.3.13 GitHub.cli`, then `gh auth login` | scripts, publishing |

The release workflow pins the same tool versions.

Notes:

- **zig version.** zig 0.16 fails cargo-zigbuild 0.24's compiler probe; 0.14.1 and 0.15.2 work. Do not use the
  `ziglang` pip package with the Microsoft Store Python: the Store sandbox virtualizes `AppData`, and zig then fails
  with `CacheCheckFailed`.
- **Why MSYS2.** libssh2 (git2's `ssh`) needs OpenSSL on macOS and Linux, and `git2`'s `vendored-openssl` builds it
  from source with Perl's `./Configure` and `make`. Git for Windows' Perl is missing modules Configure needs and ships
  no `make`. The scripts find MSYS2 in `C:\msys64` (or `MSYS2_ROOT`) and use only its `perl` and `make`; the shell
  stays Git Bash.
- **libdbus** is built from source too (keyring's `vendored` feature), so the Linux build needs no Linux sysroot.
- The Linux binary links against glibc 2.31 (Ubuntu 20.04, Debian 11 and newer); `package-linux.sh` sets it.

**macOS SDK.** Cross-linking the macOS app needs Apple's frameworks: an extracted `MacOSX<version>.sdk` folder (from
Xcode or the Command Line Tools, `/Library/Developer/CommandLineTools/SDKs/`). Copy it to the build machine and set
`SDKROOT` to it in `release.env`. Check the Xcode license terms for your use. On a Mac, `package-macos.sh` builds
natively with the installed SDK instead (that is what CI does).

Then create the configuration file:

```bash
cp scripts/release/release.env.example scripts/release/release.env
```

`release.env` is git-ignored and holds paths and passwords; `release.sh` and `publish-github.sh` source it when it
exists (CI sets the same names in the environment instead). Keep the key files outside the repository (e.g.
`~/.gityas-keys/`).

### 3. Update signing key (required)

This key proves an update came from you. The app ships the public half and refuses any bundle not signed by the
private half, so even someone who could change the release files cannot push malware to users.

```bash
cargo packager signer generate --path ~/.gityas-keys/update.key
```

| `release.env` | CI (§1) | Value |
|---|---|---|
| `UPDATE_PRIVATE_KEY_PATH` | secret `UPDATE_PRIVATE_KEY` (the file's contents) | `update.key` |
| `UPDATE_PRIVATE_KEY_PASSWORD` | secret `UPDATE_PRIVATE_KEY_PASSWORD` | the password you chose |
| `UPDATE_PUBKEY` | variable `UPDATE_PUBKEY` | contents of `update.key.pub` (one base64 line starting `dW50cnVzdGVk`) |
| `RELEASE_REPO` | the repository itself | `<owner>/<repo>` the app checks for updates |

`UPDATE_PUBKEY` and `RELEASE_REPO` are compiled into every build: changing either later strands installed copies on
the old one. With the wrong key compiled in as `UPDATE_PUBKEY`, the app finds the update but every download fails with
`update check failed: Invalid symbol 95, offset 2.` in its log, and that build can never update itself: users must
reinstall a fixed build by hand. `release.sh` refuses a value that is not a minisign public key, and refuses a bundle
signed by a different key.

Back the private key up somewhere safe (a password manager). **If you lose it, installed copies can never be updated
again**: they only trust the public key they were built with. Never commit it.

### 4. Windows code signing (recommended)

Unsigned installers trigger SmartScreen's "Windows protected your PC" warning. Updates work either way (they are
verified by the update signature), but first installs look untrustworthy. For local builds, pick one:

- **A certificate in your certificate store** (a .pfx, or a hardware token whose certificate appears in
  `certmgr.msc` → Personal). Import a .pfx once with `Import-PfxCertificate -FilePath cert.pfx -CertStoreLocation
  Cert:\CurrentUser\My` (PowerShell) and set `WINDOWS_CERTIFICATE_THUMBPRINT` to its thumbprint
  (`Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert`). Optional `WINDOWS_TIMESTAMP_URL` (default DigiCert's).
- **A signing service** (Azure Artifact Signing, formerly Trusted Signing; or your CA's cloud HSM). Install and sign
  in to its tool, and set `WINDOWS_SIGN_COMMAND` to a command that signs the file passed as `%1`.

`package-windows.sh` adds the matching `[package.metadata.packager.windows]` settings to `src-frontend/Cargo.toml`
for the build and restores the file afterwards; cargo-packager then signs `gityas.exe` and the installer with signtool.

For CI, the free option for open-source projects is [SignPath Foundation](https://signpath.org) (apply once the
repository is public). It signs through its own GitHub Action rather than a local command, so it will need its own
step in `release.yml`.

### 5. macOS signing and notarization (strongly recommended)

Without it, the app is signed ad hoc and Gatekeeper blocks the first launch ("Apple could not verify GitYas…");
users get past it with System Settings → Privacy & Security → Open Anyway. Needs an Apple Developer Program
membership. All of it works from Windows:

1. **Developer ID Application certificate.** If you have one in a Mac's Keychain, export it with its private key as
   a .p12. Without a Mac, create a key and signing request with OpenSSL (Git Bash includes it), upload the
   request at [developer.apple.com → Certificates](https://developer.apple.com/account/resources/certificates)
   (type *Developer ID Application*, G2 Sub-CA), download the `.cer` and combine it with the key into a .p12:
   ```bash
   openssl req -new -newkey rsa:2048 -nodes -keyout ~/.gityas-keys/developer-id.key \
     -subj "/CN=Yasir Fayrooz/emailAddress=you@example.com" -out developer-id.csr
   openssl x509 -inform der -in developerID_application.cer -out developer-id.pem
   openssl pkcs12 -export -inkey ~/.gityas-keys/developer-id.key -in developer-id.pem -out ~/.gityas-keys/developer-id.p12
   ```
   Set `APPLE_P12_PATH` and `APPLE_P12_PASSWORD` (CI: `APPLE_P12_BASE64` and `APPLE_P12_PASSWORD`, §1).
2. **App Store Connect API key** for notarization: App Store Connect → Users and Access → Integrations → Keys, role
   *Developer*. Download `AuthKey_<KEY_ID>.p8` (only once) and note the issuer ID. Encode them for rcodesign:
   ```bash
   rcodesign encode-app-store-connect-api-key -o ~/.gityas-keys/app-store-connect.json <ISSUER_ID> <KEY_ID> AuthKey_<KEY_ID>.p8
   ```
   Set `APPLE_API_KEY_JSON` to that file (CI: the secret `APPLE_API_KEY_JSON` holds its contents).

`package-macos.sh` then signs with the hardened runtime and a secure timestamp (`rcodesign sign
--for-notarization`), submits the app to Apple's notary service, waits and staples the ticket to the app, so it opens
without a network check. Notarization usually takes a few minutes per architecture.

### 6. Linux

Nothing to set up beyond the build tools. AppImages and .debs carry no OS signature; updates are still verified by
the update signature. The AppImage runtime is downloaded once (pinned by checksum) to `target/release-tools/`.

## Releasing a version

1. **Version**: set `version` under `[workspace.package]` in the root `Cargo.toml` (e.g. `1.2.0`). Versions must
   only go up: the updater installs a manifest version only when it is greater than the running one.
2. **Changelog**: in `CHANGELOG.md`, rename `## [Unreleased]` to `## [1.2.0] - 2026-11-02` and start a new empty
   `## [Unreleased]` above it.
3. **Check**: CI is green on `main` (or run `scripts/check.sh`).
4. **Tag**: commit, then `git tag v1.2.0 && git push origin main v1.2.0`. The tag starts the release workflow.
   (*Actions → Release → Run workflow* rebuilds an already-pushed tag.)
5. **Review the draft**: when the workflow finishes, *Releases* has a draft `v1.2.0` with every installer, update
   bundle, `.sig` and `latest.json`. Download and smoke-test the installers, ideally on each OS (a VM is enough for
   Linux).
6. **Publish** the draft. Running copies update on their next start.
7. **Pulling a bad release**: mark it as a pre-release (or delete it); `releases/latest` then points at the previous
   release again. Clients already on the bad version stay there: ship a fixed, higher version. Never replace the files
   of a published release: installed apps may already hold its `latest.json`, and a rebuilt bundle no longer matches
   its signature. `publish-github.sh` refuses to touch a published release.

**Releasing from your own machine instead** (steps 4–6): tag and push as above (the workflow starts too; cancel it
in *Actions* if you don't want both), then:

```bash
scripts/release/release.sh 1.2.0        # all four platforms into dist/v1.2.0/, plus latest.json
scripts/release/publish-github.sh 1.2.0  # draft release v1.2.0 on RELEASE_REPO
```

To rebuild one platform: `scripts/release/release.sh 1.2.0 linux` (this clears `dist/v1.2.0/`, so build all of them
before publishing). A draft can be refilled with `RELEASE_REUPLOAD=1 scripts/release/publish-github.sh 1.2.0`.
Test builds without a tag: `RELEASE_ALLOW_DIRTY=1 scripts/release/release.sh 1.2.0 linux`.

## The changelog

[CHANGELOG.md](../CHANGELOG.md) at the repository root is the single source of release notes:

- It is compiled into the app ([changelog.rs](../src-frontend/src/ui/changelog.rs)), so each build shows the notes it
  shipped with, works offline and needs nothing on the server.
- After an update, *What's new in GitYas* opens once with every version newer than the one the user last saw (the
  user DB remembers it as the `last_seen_version` setting), each marked *NEW*; *Show all versions* expands to the
  whole history. Fresh installs skip it.
- Clicking **GitYas vX.Y.Z** in the bottom-left corner opens the full changelog any time.
- The same section becomes the `notes` in `latest.json` and the GitHub release's description.

Format: one `## [X.Y.Z] - YYYY-MM-DD` section per version (`## X.Y.Z` and `## vX.Y.Z` also work), with `###`
subheadings, lists, **bold**, *italic*, `code`, links and code blocks. `## [Unreleased]` is never shown.
`prepare.sh` fails if the version has no section, so a release cannot go out without notes.

Why Markdown rather than, say, JSON or notes fetched from the server: it is pleasant to write, reads well on GitHub,
needs no extra tooling, and compiling it in avoids a network request and keeps old builds' notes accurate. The
trade-off is that notes cannot be corrected after a build ships; fix them in the next release.

## Testing an update locally

1. Build a "current" version with a local endpoint and a test key:
   ```bash
   cargo packager signer generate --ci --password test --path /tmp/test.key
   export CARGO_PACKAGER_SIGN_PRIVATE_KEY="$(cat /tmp/test.key)" CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD=test
   export GITYAS_UPDATE_ENDPOINT=http://127.0.0.1:8765/latest.json GITYAS_UPDATE_PUBKEY="$(cat /tmp/test.key.pub)"
   cargo packager --release -p gityas-frontend -f nsis
   ```
   Install it. (For macOS or Linux, run `scripts/release/package-macos.sh aarch64 <version> <dir>` or
   `package-linux.sh <version> <dir>` with the same variables set.)
2. Bump the version, rebuild the same way, and put the new bundle, its `.sig` and a hand-written `latest.json`
   (see `scripts/release/manifest.sh` for the shape; URL `http://127.0.0.1:8765/<file>`) in one folder.
3. Serve that folder (`python -m http.server 8765`) and start the installed old version.
