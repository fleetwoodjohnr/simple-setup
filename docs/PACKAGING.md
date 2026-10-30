# Packaging, signing and installing

## What a release contains

`scripts/build-release.sh` builds, for the operating system it runs on:

| File | What it is |
|---|---|
| `Simple Setup_<v>_amd64.deb`, `Simple Setup-<v>-1.x86_64.rpm`, `Simple Setup_<v>_amd64.AppImage` | Linux desktop app (x86-64; the ARM64 runner builds the same for aarch64) |
| `Simple Setup_<v>_x64_en-US.msi`, `Simple Setup_<v>_x64-setup.exe` | Windows desktop app (MSI and NSIS installer; ARM64 gets the NSIS installer only) |
| `Simple Setup_<v>_aarch64.dmg` / `_x64.dmg`, `Simple-Setup-<v>-macos-<arch>-app.tar.gz` | macOS desktop app (the archive is what the updater installs) |
| `simple-setup-<v>-<os>-<arch>[.exe]` | The CLI alone, for every platform in the kit (also the restore program) |
| `restore-kit-<v>.tar.gz` | Restore programs for every platform, as copied into snapshots |
| `SHA256SUMS` | Checksums of all of the above |

Every desktop package carries the CLI next to the app and the complete restore kit
(`restore-kit/<os>-<arch>/simple-setup[.exe]` for Linux, Windows and macOS on x86-64 and ARM64),
so a snapshot made on any computer can include a restore program for any other. Installed
locations: `/usr/bin/simple-setup{,-gui}` and `/usr/lib/Simple Setup/restore-kit/` (deb, rpm,
AppImage), `Simple Setup.app/Contents/{MacOS,Resources/restore-kit}` (macOS), the install folder
(Windows). `build::kit_dirs` looks in exactly these places.

## Building

```sh
npm ci
./scripts/build-release.sh                   # this OS; kit = what this computer can build
KIT=path/to/kit TARGET=x86_64-apple-darwin ./scripts/build-release.sh   # complete kit, other arch
```

`scripts/check-release.sh` then checks the result: the expected packages exist, they contain the
app icon, the CLI and the restore kit, `SHA256SUMS` verifies, and this computer's restore program
runs.

Locally the kit holds the Linux restore programs (static musl, x86-64 and ARM64, built with
`rust-lld`) and, if `cargo-xwin` and the `*-pc-windows-msvc` targets are installed, the Windows
ones. macOS restore programs need a Mac. The workflow `.github/workflows/release.yml` ("Build &
Release") runs on every pull request and push to `main`: it builds all six restore programs
first, then the packages on Linux, Windows and macOS (both architectures each, Windows ARM64
natively on `windows-11-arm`) with the complete kit, checks them with `check-release.sh`, and
keeps them as downloadable run artifacts.

## Publishing a release

1. Set the same version in `Cargo.toml` (workspace) and `package.json`, e.g. `0.2.0`.
2. Push the tag `v0.2.0`. The workflow refuses a tag that does not match the version.
3. The release is **published at once** with generated notes and every file plus one
   `SHA256SUMS`. A tag with a suffix (`v0.2.0-rc1`) is published as a **prerelease**.

Installed copies of Simple Setup find the release through GitHub's "latest release", which
skips drafts and prereleases, so only `vX.Y.Z` tags reach users' updaters.

## Signing and notarization

Unsigned builds work, but each OS warns about them (see below). The release workflow signs when
these repository secrets exist and skips signing otherwise:

| Secret | Used for |
|---|---|
| `APPLE_CERTIFICATE` (base64 .p12), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` ("Developer ID Application: …") | `codesign --options runtime --timestamp` of the restore programs and the app (Tauri signs the app and its sidecar) |
| `APPLE_ID`, `APPLE_PASSWORD` (app-specific password), `APPLE_TEAM_ID` | `notarytool submit --wait` for the restore programs; Tauri notarizes and staples the app/DMG |
| `WINDOWS_SIGN_COMMAND` | A command that signs the file appended to it, e.g. Azure Trusted Signing (`trusted-signing-cli …`) or `signtool sign /fd sha256 /tr <timestamp-url> /td sha256 /f cert.pfx /p …`. For the installers, set `bundle.windows.signCommand` in `src-tauri/tauri.release.conf.json` to the same command (`%1` is the file). |

Linux packages are not signed; users verify `SHA256SUMS` (and, if a maintainer publishes one, a
signature of it, e.g. `minisign -Vm SHA256SUMS`). Restore programs inside a snapshot are copies of
the release's (signed) programs, so a snapshot made with a signed release restores without
warnings on Windows and macOS.

## Installing

- **Linux:** `sudo dnf install ./Simple*Setup-*.rpm`, `sudo apt install ./Simple*Setup_*.deb`, or
  make the AppImage executable and run it. Verified here: the rpm on Fedora 44 and the deb on
  Ubuntu 24.04 install with all libraries resolved, and their CLI finds every bundled restore
  program.
- **Windows:** run the MSI or the setup program. Without a signature, SmartScreen shows "Windows
  protected your PC": choose More info, then Run anyway.
- **macOS:** open the DMG and drag Simple Setup to Applications. Without notarization, macOS
  refuses to open it the first time: right-click, Open, then Open (or System Settings → Privacy &
  Security → Open Anyway).

Check downloads first: `sha256sum -c SHA256SUMS --ignore-missing` (macOS: `shasum -a 256 -c`).

## Prompts when restoring from a snapshot

- **Windows:** double-click `launchers\restore-windows.cmd`. Files copied from a USB drive or
  received with Simple Setup carry no "downloaded from the internet" mark, so SmartScreen does
  not intervene; if the snapshot was downloaded or unzipped from an email, Windows may block the
  restore program: right-click it, Properties, Unblock (or More info, Run anyway). Installers
  that need administrator rights trigger the normal Windows consent prompt. `restore-windows.ps1`
  needs `powershell -ExecutionPolicy Bypass -File …` because PowerShell blocks scripts by default.
- **macOS:** double-click `launchers/restore-macos.command`. Copies from a drive are not
  quarantined; if the snapshot was downloaded, Gatekeeper blocks the unsigned restore program:
  right-click, Open, or remove the quarantine yourself (`xattr -dr com.apple.quarantine <snapshot>`).
  Simple Setup never removes it for you.
- **Linux:** `sh launchers/restore-linux.sh`. Drives mounted without execute permission are
  handled: the launcher runs a private copy of the restore program.
- **Receiving over the network:** the first time a computer receives, Windows Defender Firewall
  and the macOS application firewall ask whether Simple Setup may accept incoming connections.
  Allow it on private networks only. Discovery uses mDNS (built into Windows 10+, macOS and
  Simple Setup itself; no Avahi needed); where it is blocked, type the address the receiver
  shows.
