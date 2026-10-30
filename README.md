# Simple Setup

Scan a computer, **choose** which apps, app settings/data and personal files to bring, build a
portable snapshot on an external drive or folder (or send it over the local network), and
restore it on another computer — Linux, Windows or macOS, the same system or a different one.
Apps are reinstalled from the snapshot without internet, or else with the new computer's own
package managers, verified downloads, or a build from source; settings move through per-app
converters. A snapshot is an app-and-data migration bundle, not a disk image. Anything that cannot
be restored on the target is reported and skipped, never silently replaced, and never stops the
rest. How to use it: [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

Desktop app (Tauri 2 + React) and CLI share one Rust engine (`crates/core`). The CLI binary is
also the restore engine copied into every snapshot, so a fresh target needs no Python, Node or Rust.

## Status (v0.2, milestones M1–M8)

| Works now | Not yet (see docs/ARCHITECTURE.md) |
|---|---|
| Restore on any system: apps without a usable installer in the snapshot are installed with the new computer's package managers (Flathub per user, winget, Homebrew, APT/DNF/Zypper/Pacman in one elevated batch, snap) or verified downloads (winget catalog, Homebrew cask API, the app's GitHub release: AppImage, MSI, NSIS/Inno, disk image, app zip, single-program archives), matched by exact IDs only; a registry of 100 apps checked against the live catalogs | Fuzzy matching of unknown apps (deliberately not done); Homebrew itself is not installed for you |
| Source builds of GitHub checkouts at their recorded commit (Cargo, Go, pipx), with the build tool installed first | make/CMake/npm builds |
| One-pass restore: batched password prompt, downloads kept for resuming (and for offline use later), a report that records what was used and survives cancellation, Windows restart detection, Retry failed; `receive --restore` / "Restore automatically when it arrives" | Automatic continuation after a reboot (run the restore again; finished items are skipped) |
| Self-update from GitHub releases with checksum verification, for AppImage, deb, rpm, MSI, NSIS and the macOS app | Not exercised against a real release yet (none is published) |
| Settings page (theme, updates, locations, restore options), System/Light/Dark themes, new icon, redesigned dashboard and restore preview | |
| Linux discovery: Flatpak, RPM/DNF, Snap, AppImage (incl. embedded GitHub update info), desktop shortcuts to custom installs, Git checkouts in folders you approve | |
| Windows discovery: installed programs (registry), Microsoft Store/MSIX, Scoop, Chocolatey. macOS discovery: `.app` bundles (incl. App Store), Homebrew formulae and casks, MacPorts, installer receipts. Settings/data locations from the app registry and bundle IDs | Not yet run on a real Windows or Mac (fixture tests, cross-target type checks and Wine runs here; CI scans real runners) |
| DPKG/APT and Pacman parsers (fixture-tested only; no Debian/Arch machine was available) | Offline DPKG/Pacman package sets. These apps get manual instructions (or their GitHub AppImage), and the snapshot is not labelled offline-ready |
| Manual corrections and hand-added apps, including your own installer link + SHA-256 per target platform; detected / guessed / entered-by-you is shown per field | |
| Installers for other platforms, downloaded while building and checked against the published SHA-256: Windows from winget's catalog, macOS from Homebrew casks, otherwise the app's GitHub release (AppImage, MSI, disk image). Windows MSI/NSIS/Inno/WiX installers run unattended and are confirmed in the installed-programs list; macOS apps are copied into Applications and confirmed by bundle ID | Installing on a real Mac (only type-checked); Homebrew formulae; `.exe` installers of unknown type, `.msix` and `.pkg` are bundled for you to open |
| Settings moved between operating systems by per-app adapters: VS Code, Git, Firefox, Thunderbird, GitHub CLI, Neovim. Machine-specific files stay in the snapshot and are listed | Settings of other apps on another OS are kept in the snapshot and reported as unsupported |
| 4-step Snapshot Builder: per-app reinstall/settings/data, sub-path excludes, personal folders with a folder tree, secrets withheld unless opted in, targets, saved profiles, size/space/coverage preflight | |
| Build: staged copy, SHA-256 of every file, read-back verification, resume after cancel/failure, atomic finish, launchers and the restore engine for selected targets | |
| Optional encryption: password plus a one-time recovery key; contents, file names, manifest and reports are encrypted, and integrity can be checked without the password | Installer files are not encrypted; the password of an existing snapshot cannot be changed |
| Updates of an existing snapshot: unchanged files are hard-linked (or copied) from it, installers reused when the app version is unchanged | |
| Sending to another computer on the local network: it announces itself only while receiving and shows an 8-digit code; pairing (SPAKE2) and an encrypted, authenticated stream; the receiver accepts the named offer; every file is checked on arrival; interrupted transfers resume | Sending straight from the build (a snapshot is first saved to a folder); transfers beyond the local network |
| Snapshots: list, inspect, verify, restore, reuse choices, delete (type the name to confirm) | |
| Release packages for Linux, Windows and macOS (x86-64 and ARM64), each with restore programs for all six platforms; checksums; signing and notarization when certificates are configured (docs/PACKAGING.md). Built and checked on every pull request; published on `vX.Y.Z` tags | The release workflow has not run on GitHub yet; no real signing certificates |
| Accessibility: WCAG 2.2 AA checks (axe-core) on every page and dialog in both themes, keyboard use, reflow, 200% zoom (docs/ACCESSIBILITY.md) | Not tested with a screen reader, or on Windows/macOS webviews |
| Restore: integrity check, per-item status with reasons, dry run, no-overwrite (skip or keep both), report | |
| Offline installers (Linux): Flatpak bundles with their runtime and GL/codec extensions, RPM packages with their full dependency set, snaps with base and content providers, AppImages and self-contained binaries. Installed on the target without internet; administrator password only for dnf/snap; each app counts as installed only after the package manager confirms it | Snap restore is untested (no snapd in the test containers) |
| Static (musl) restore programs for Linux x86_64 and aarch64 (`scripts/build-restore-kit.sh`) | The aarch64 program was built but not run (no ARM machine or emulator here) |

## Build and run

Requires Rust 1.90+, Node 20+, and the [Tauri 2 Linux prerequisites](https://tauri.app/start/prerequisites/)
(`webkit2gtk4.1-devel`, `gtk3-devel`, `libsoup3-devel` on Fedora).

```sh
npm install
cargo build -p simple-setup-cli          # the CLI / restore engine (target/debug/simple-setup)
./scripts/build-restore-kit.sh           # static Linux restore programs (x86_64 + aarch64) in target/release/restore-kit,
                                         # used by target/release/simple-setup; needs the two musl rustup targets
npm run tauri dev                        # desktop app (finds the engine next to its own binary)
npx tauri build --debug --no-bundle      # self-contained debug build of the desktop app
./scripts/build-release.sh               # release packages + CLI + restore kit + SHA256SUMS in release/
```

Installing, signing and the prompts each OS shows are described in [docs/PACKAGING.md](docs/PACKAGING.md).

## CLI

```sh
simple-setup scan [--json] [--source-dir ~/projects]
simple-setup volumes
simple-setup schema                                       # JSON Schema of simple-setup.json
simple-setup profile show "Work Setup" > sel.json         # selections are explicit JSON; nothing is chosen for you
simple-setup snapshot preflight --selection sel.json --dest /run/media/me/USB
simple-setup snapshot create --selection sel.json --dest /run/media/me/USB --name "Work laptop"
simple-setup snapshot verify "/run/media/me/USB/Work laptop"
simple-setup snapshot list | show | delete | discard-partial
simple-setup restore "/run/media/me/USB/Work laptop" --dry-run
simple-setup restore "/run/media/me/USB/Work laptop" [--only r2] [--keep-both] [--password-file f]
simple-setup restore ... --offline                        # only what is in the snapshot: no downloads
simple-setup restore ... --no-source-builds               # never build apps from source
simple-setup snapshot create ... --encrypt                # asks for a password, prints a recovery key once
simple-setup snapshot create ... --base "/run/media/me/USB/Work laptop"   # update: only changes are copied
simple-setup receive [--dest DIR] [--restore [--yes]]     # new computer: shows a pairing code, waits;
                                                          # --restore restores as soon as it arrives
simple-setup peers                                        # computers nearby that are receiving
simple-setup send "/run/media/me/USB/Work laptop" [--to 192.168.1.20:41234] [--code 1234-5678]
```

On the target, from the snapshot folder: `sh launchers/restore-linux.sh --dry-run`, then without `--dry-run`.

## Tests

```sh
cargo test --workspace     # parsers on fixtures, path safety, secret rules, registry, build → verify → restore round trip
npm test                   # selection logic and wizard gating
e2e/run.sh a11y            # the real app via WebDriver: accessibility audit (needs tauri-driver, WebKitWebDriver, Xvfb)
e2e/run.sh acceptance      # the real app: encrypted snapshot, send, receive (restoring automatically), unlock, update
cargo test -p simple-setup-core -- --ignored live         # real GitHub: update check, resumable download
GITHUB_TOKEN=$(gh auth token) node scripts/check-registry.mjs   # every registry ID against the live catalogs
./scripts/check-release.sh                                # packages: files, icon, CLI, restore kit, checksums
```

What was verified on Fedora 44 x86_64: real read-only scan (70 apps in ~3 s); the GUI acceptance
flow driven through WebDriver (`tauri-driver`) against a fixture home: two apps + Documents → build →
restore; and the bundled launcher restoring into a fresh home inside a network namespace with no
network.

M3 (offline installers), with a fixture home so no personal files were copied:

- The CLI built a snapshot of the real Spotify Flatpak, the `gh` RPM and an AppImage in 12m43s
  (567 MB). It held five Flatpak bundles (runtime, two GL driver branches, codecs, app) and 83 RPMs
  with signatures verified by `rpmkeys`. `sha256sum -c` passed, and the GitHub token in
  `hosts.yml` was withheld.
- In a fresh Fedora 44 container with `--network none`, `restore-linux.sh --yes` installed
  Spotify, `gh` (9 missing packages, `gh --version` runs) and the AppImage. Each install was
  confirmed by `flatpak info`, `rpm -q` or a checksum, and the settings were restored in 42 s.
  A second run changed nothing.
- On Ubuntu 22.04 (static engine, no Flatpak or DNF), the AppImage and settings were restored,
  and Spotify and `gh` were reported as manual steps. On Fedora 43 the dry run refused the
  Fedora 44 packages.
- In the GUI flow with `gh`'s installer selected, the 83 packages were collected, and restoring on
  this machine (where `gh` is installed) only confirmed the install. No password prompt appeared
  and `dnf` was not run.

M4 (Windows and macOS discovery):

- The Windows build (cross-compiled with `cargo xwin`) ran under Wine 11.20 in a container with
  `--network none`. All 29 unit tests and 3 round-trip tests passed. The run found two
  Windows-only bugs, now fixed: settings folders listed twice on case-insensitive drives, and test
  folders under `AppData\Local\Temp` being skipped as temporary files.
- With test registry entries, a Scoop app and Chocolatey packages in place, the scan listed:
  - Firefox, matched to the registry, with `winget install --id Mozilla.Firefox` as its reinstall
    step
  - RustDesk, with its GitHub repository taken from its URL
  - an app with a Unicode name
  - Scoop's neovim and Chocolatey's Git

  The C++ runtime was hidden and the system component was not listed.
- The Windows CLI built a snapshot of the Firefox profile, neovim's `persist` folder and Documents,
  then verified it. After the originals were moved away, the restore brought the files back
  identical and kept `logins.json` out. The installers were listed as manual steps with the exact
  command. A second restore changed nothing and saved a separate report.
- The macOS readers are fixture-tested on Linux: binary and XML `Info.plist`, App Store receipt,
  Homebrew JSON, MacPorts and installer receipts. `cargo clippy --target aarch64-apple-darwin` is
  clean.
- CI now runs a real read-only scan on every runner (Linux, Windows, macOS) and fails if it finds
  none of the expected app sources (`scripts/check-scan.mjs`).

M5 (installers and settings for other systems), with a fixture home and fake settings:

- A snapshot of the real `gh` and KeePassXC RPMs and the RustDesk Flatpak was built on this
  Fedora machine for Windows x86_64, macOS ARM64 and Linux ARM64 in 44 s. It downloaded:
  - from winget: the gh and KeePassXC MSIs
  - from RustDesk's GitHub release: its Windows MSI and ARM64 AppImage. RustDesk is not in
    winget, so the lookup fell back to GitHub.
  - from Homebrew casks: the KeePassXC and RustDesk disk images

  Every file matched its published SHA-256. gh has only a Homebrew formula, so it is listed as a
  manual step for macOS. KeePassXC settings have no converter, so they are reported as
  unsupported. The fake gh token was withheld.
- In a Wine container with `--network none`, the snapshot's own Windows engine:
  - installed all three MSIs, each confirmed in the installed-programs list; `gh --version` ran
  - converted gh's `config.yml` into `AppData\Roaming\GitHub CLI`
  - restored Documents, and put nothing in Linux-style folders
  - changed nothing on a second run
- A core test restores Linux VS Code, Git and Firefox settings into a Windows home. It checks
  that `keybindings.json` and `compatibility.ini` stay behind, the `.gitconfig` credential
  helper is disabled, `[Install…]` is removed from `profiles.ini`, and a second run is
  idempotent. All 36 unit and 4 round-trip tests pass on Linux and as Windows builds under Wine.
- Checking the registry against the live catalogs found two stale entries (RustDesk in winget,
  Homebrew's `docker` cask), now fixed.

M6 (network transfer, encryption, updates):

- Two Fedora containers on a private podman network, using the static Linux engine. `receive`
  announced the computer and showed a code. `peers` and `send` found it by mDNS without an
  address. A wrong code was refused on both sides. The right code transferred an encrypted
  snapshot, which the receiver accepted, checked and saved. In a third container with no
  network, `restore --password-file` restored Documents and the converted gh settings.
  Without a password it stops and explains how to give one.
- The encrypted snapshot holds no readable file names, contents or recovery key. The core test
  checks this byte for byte, and `snapshot verify` passes without the password.
- An update built with `--base` took the unchanged gh settings file from the earlier snapshot
  (a hard link, checked by inode in the test), copied the changed and new files, stayed
  encrypted with the same key, and verifies.
- In the desktop app (driven through WebDriver, headless):
  - built an encrypted snapshot; the recovery key was shown once
  - sent it to a CLI receiver found by mDNS
  - received it back from a CLI sender after accepting its offer
  - unlocked it with the password and restored a deleted file
  - built an encrypted update that reused the unchanged file
- Core tests cover a wrong code, a transfer cut off midway that resumes from the partial file,
  and the offer being asked once. All 40 unit and 6 round-trip tests pass on Linux and as Windows
  builds under Wine, including the network transfer over loopback.

M7 (release, packaging, accessibility):

- `scripts/build-release.sh` built the deb, rpm and AppImage here, along with the CLI for Linux
  x86-64/ARM64 and Windows x86-64, the restore-kit archive and `SHA256SUMS`.
  - The rpm installed on a fresh Fedora 44 and the deb on a fresh Ubuntu 24.04, with every
    library of the app resolved.
  - In both, the installed CLI found all three bundled restore programs and built a snapshot that
    was offline-ready for Linux x86-64, Linux ARM64 and Windows.
  - The AppImage contains the same files.
- The new `restore-windows.cmd` ran under offline Wine from a snapshot whose folder name has a
  space: a dry run, then a restore.
- The accessibility audit ran the real app in a virtual display.
  - Before the fixes it found two contrast failures in the light theme and one target-size
    failure; earlier checks had only seen the dark theme.
  - After the fixes, all 34 audits pass (17 states × 2 themes), along with the keyboard, focus,
    Escape, reflow and 200% zoom checks.
- `e2e/run.sh acceptance` passes in the repo version of the GUI flow (with the receiver found by
  mDNS). The runner now starts and stops its own X server, after an earlier version left 18 of
  them running.

M8 (restore anywhere, self-update, UI), with fixture homes and throwaway containers:

- The CI `gui` job failed after all its checks passed: `e2e/run.sh`'s exit trap used
  `kill -TERM -- -PGID`, which Ubuntu's `sh` (dash) rejects with status 2. Reproduced and fixed in an
  `ubuntu:24.04` container; both workflows pass actionlint.
- Fedora 44 container, restoring a snapshot built here (as root, with network):
  - Git through one DNF batch;
  - ripgrep from its GitHub tarball, bundled offline;
  - cargo-llvm-lines, which has no release binaries, built from source at the recorded commit after
    the Rust toolchain was installed with DNF (`cargo-llvm-lines --version` runs);
  - a snapshot made only for ARM64 installed ripgrep for x86-64 online, checking the SHA-256;
  - a second restore changed nothing.
- Ubuntu 24.04 container (empty package lists, no curl): Git through the single elevated APT
  script (lists refreshed first); curl installed as a dependency, then ripgrep downloaded and
  verified.
- Windows under Wine (`--network none`): the GitHub CLI MSI and ripgrep's `rg.exe`, downloaded on
  Linux for the Windows target, were installed from the restore's download folder; the MSI was
  confirmed in the installed-programs list. Wine's TLS stack cannot run curl, so live downloads on
  Windows were not exercised here (the same code ran live on Linux).
- `scripts/check-registry.mjs`: 519 IDs (100 apps, 41 GitHub repositories) found in the live
  catalogs after fixing 5 stale ones (Thunderbird and OrcaSlicer Flathub IDs, DBeaver's winget ID,
  FileZilla no longer in winget, FreeCAD no longer in Fedora).
- Update check against the real repository ("no release published yet"); a real GitHub download
  interrupted and resumed, and a damaged partial file re-fetched.
- The desktop app: `e2e/run.sh a11y` 38 of 38 audits pass (Settings, theme switching, the update
  check included); `e2e/run.sh acceptance` passes, now with automatic restore after receiving.
- 56 unit tests (plus 2 live network tests run by hand) and 7 round-trip tests; clippy clean for
  Linux, Windows and macOS targets.

Not run:
- the workflows on GitHub (they run when this is pushed), the first release and an update from
  it, and signing with real certificates
- installs on a real Mac or Windows PC (winget, Homebrew, UAC prompts), Flathub and snap installs
- a transfer between two physical computers or across Wi-Fi
- the Windows and macOS firewall prompts for the receiving port
- installing from a disk image or zip on macOS
- MSIX/Store discovery (needs PowerShell, which Wine lacks)
- the `restore-windows.ps1` launcher, and Windows' consent prompt for installers that need
  administrator rights (Wine runs everything as administrator)
- the macOS engine (type-checked only) and the desktop app on Windows or macOS (built only by CI)
- the aarch64 engine and the downloaded ARM64 AppImage (no ARM machine or emulator here)
- `pkexec`/`sudo` prompts (container tests ran as root; nothing was installed on this machine)
