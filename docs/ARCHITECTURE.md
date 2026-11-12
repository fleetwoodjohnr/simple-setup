# Architecture

## Decision

**Rust core + Tauri 2 shell + React UI**, matching the existing `simple-connect` project.

| Choice | Why |
|---|---|
| All logic in `crates/core` (no Tauri dependency) | One engine for GUI and CLI; unit-testable; the GUI cannot simulate anything the CLI can't do. |
| CLI (`crates/cli`) is also the restore engine | A ~5–10 MB native binary per target runs on a fresh machine with no runtime. The webview app is not needed to restore. |
| Tauri 2 | Small native shell using the system webview (WebKitGTK, WebView2, WKWebView); IPC allow-list generated in `src-tauri/build.rs`; no shell/fs/http plugins exposed to the webview. |
| JSON manifest (`simple-setup.json`) | The spec allows JSON; serde + schemars give a typed, versioned schema (`simple-setup schema`) without an unmaintained YAML crate. |
| Discovery adapters run read-only listing commands and read metadata files | No root, no network, no code execution (Git metadata is parsed from files; `git` is never run). |

Boundaries, in the order a migration uses them:

| Concern | Module | Owns |
|---|---|---|
| Operating system detection | `platform.rs`, `probe.rs` | os-release, device, processor, kernel (diagnostics), tools on PATH, Flathub, FUSE, image-based systems; `SystemInfo::assumed` for a computer that is not here |
| Distribution registry | `distros.rs` + `registry/distros.json` | profiles with inheritance, matching, validation, trust, user/official layers |
| Application discovery | `discovery/` | what is installed (the package database the host profile names) |
| Application identity | `registry.rs` + `registry/apps.json` | one app → its IDs per package manager and distribution family, settings paths |
| Package manager adapters | `resolve.rs` (system), `installers.rs` (bundled recipes) | fixed command lines per adapter; trusted custom adapters |
| Compatibility analysis | `compat.rs` | `bundle_fits`, ranked candidates with provenance and explanations, verdicts |
| Snapshot creation | `build.rs`, `manifest.rs` | coverage per destination, installer collection, format 2 + migration |
| Transfer and deployment | `transfer.rs`, `restore.rs` | pairing, probe, send/receive; plan and execute |
| UI presentation | `src/` (React), `src-tauri` (IPC) | the guided flows; no decisions are made in the UI |

No module outside `distros.rs` and the profiles knows a distribution by name: Linux Mint's snap
policy, Ubuntu's snap-only Firefox package or openSUSE's package names are profile and registry
data, not code.

```
crates/core/src
  platform.rs   OS/arch/device, os-release parsing, volumes, free space (sysinfo)
  distros.rs    distribution profiles: schema, inheritance, os-release matching, validation,
                import/export, trust of custom commands, official updates
  compat.rs     the compatibility engine: which bundled installers fit a destination, ranked
                install candidates (profile rules, preferences, repository provenance), verdicts
  discovery/    mod.rs (App model, scan, raw/package-manager merge), linux.rs, windows.rs,
                macos.rs (adapters + pure parsers), git.rs
  registry.rs   + registry/apps.json: canonical app → package IDs per OS, known settings paths
  sensitive.rs  secret and cache/lock path rules (path-based, contents never read)
  selection.rs  explicit user choices; validated against the inventory
  inspect.rs    size estimates and folder listings for the UI
  installers.rs offline installers: plan, collect (Flatpak bundle, dnf download, snap download,
                program copy, catalog download), check, install with fixed argv + optional
                sudo/pkexec or Windows consent prompt, post-install check
  catalog.rs    winget manifests, Homebrew cask API, GitHub releases: parse and choose an installer
                (AppImage/MSI/NSIS/Inno/disk image/app zip/single-program archive), checksum lists,
                installer-type marks, zip listings
  fetch.rs      HTTPS through curl: lookups (GitHub token via a private header file, rate-limit
                messages, short cache), downloads verified against published SHA-256, resumable
                downloads, a connectivity check
  probe.rs      the computer being restored onto: OS, CPU, memory, free space, package managers
                and build tools on PATH, Flathub, administrator rights, internet
  resolve.rs    install methods for apps the snapshot has no installer for: Flathub, winget,
                Homebrew, APT/DNF/Zypper/Pacman (batched, one prompt each), snap, verified
                download (catalog or GitHub release), source build (Cargo/Go/pipx at the recorded
                commit, toolchain first); tried in order until one is confirmed
  update.rs     self-update from GitHub releases: check, verified resumable download, install by
                installation type (AppImage/app swap with rollback, deb/rpm, MSI, NSIS)
  adapters.rs   per-app settings converters for restoring on another OS
  crypt.rs      optional snapshot encryption (age: random X25519 key sealed with the password)
  transfer.rs   paired LAN transfer: mDNS while receiving, SPAKE2 pairing code, ChaCha20-Poly1305
                frames, per-file SHA-256 checks, resume
  build.rs      preflight/coverage, staged copy, hashing, verification, launchers, manifest
  snapshot.rs   summary, verify, delete
  restore.rs    plan (status, reason, channel and method per item), execute (installs before
                files; never aborts on one item; cancel leaves a report), report, recent reports
  store.rs      profiles, settings, user corrections, snapshot index (JSON in ~/.config/simple-setup)
crates/cli      `simple-setup` binary
src-tauri       IPC commands (thin wrappers)
src             React UI: Dashboard, Applications, Create Snapshot, Snapshots, Restore
```

## Build pipeline

`plan` (metadata only) → staging folder `.<name>.partial/` with a marker and journal → collect
installers for apps whose installer is planned for this computer's platform (journaled, so a
resumed build reuses finished ones; a failure downgrades that app to manual instructions) → copy each
file while hashing → re-read every copy and compare → copy restore engines + launchers for the
selected targets → write manifest, `checksums/SHA256SUMS`, reports → remove journal → rename into
place. Cancel or failure leaves the staging folder; building again with the same name and selection
resumes (journal entries are reused when source size and mtime match). An existing snapshot folder
is never overwritten.

## Restore pipeline

`plan`: verify → detect this computer (`probe`: os-release → profile) → one read-only query of
its package manager for every candidate name (origin repository per package) → for each app
chosen for reinstall: the snapshot's own installer for this exact platform if `compat::bundle_fits`
and the tools are there; otherwise `compat::candidates` over `resolve::choose` (exact IDs only:
the app's own package name where the destination shares its package-name space, the registry's
names for the destination profile's keys, Flathub/snap IDs, its GitHub repository; a source build
for checkouts with a supported build tool), reordered by the profile's rules and the user's
preferences, filtered by the package lists, with third-party repositories last and marked for
approval; otherwise Unsupported with the reason. Each item carries its verdict and every option
considered. Online items need
internet (else Manual "needs internet", unless an earlier restore already downloaded the
installer). Items are ordered: build tools, snapshot installs, online installs, source builds,
then settings and files. `execute`: one batched elevated install per system package manager
(plus `curl` when downloads need it) → every item in order, each method tried until one is
confirmed → files → report (always written, also when cancelled; records method, release, file,
checksum and commit).

## Planning for a computer that is not here

The same engine plans for a destination that is only described: `build::Destinations` turns each
selected target into a `SystemInfo` — the computer probed over a paired transfer (`transfer::probe`),
or `SystemInfo::assumed(platform, profile)` from the distribution the user chose (its standard
package manager, Flatpak or snapd where they come by default). Coverage rows then carry the
verdict and installation plan the destination would use, checked again against its real package
lists when it restores. `simple-setup analyze --as <profile>[:release]` runs the restore planner
against such a description (`Target::described`, no live queries).

## Risks

| Risk | Mitigation / status |
|---|---|
| "Offline-ready" overclaimed | Readiness is computed per target and is true only if every selected part is restorable from the bundle *and* an engine for that target is bundled. An installer is "ready" only for the source platform and only once its files were actually collected; a failed collection downgrades it to manual. Restore re-checks the target (tool present, same distro release for RPMs). |
| Package managers differ by distro/version | Pure parsers with fixtures; adapters skip silently when a tool is absent and note it. DPKG/Pacman untested on real systems. |
| Catalog entries go stale | M5 checked every registry winget ID and cask token against the live catalogs (two were wrong and fixed). A failed lookup falls back to the app's GitHub release, otherwise the item is reported as manual; the evidence lists the exact URL and checksum. |
| Installs on macOS cannot run here | The disk-image/zip install path is type-checked for macOS only; the choice of download is tested with real Homebrew answers. A failed install is reported as failed, never as installed. |
| Windows/macOS adapters cannot run here | Parsers are fixture-tested on Linux; core and CLI pass clippy for `x86_64-pc-windows-msvc` and `aarch64-apple-darwin`; the Windows build runs its tests, a scan, a snapshot build and a restore under Wine (offline container); CI runs `scan` on real Windows and macOS runners and fails if the main adapters find nothing (`scripts/check-scan.mjs`). |
| One program listed twice (installer entry + package manager) | Windows installed-apps entries and macOS bundles merge into the package-manager app with the same registry identity; casks claim the bundle they installed by path. Unknown programs can still appear twice; both entries say where they came from. |
| App files changing during copy | Size/mtime re-checked after copying; a warning asks to close the app. |
| Secrets leaking into bundles | Path rules withhold keys, keyrings, password DBs, browser logins, tokens; opt-in per path with a warning. Optional age encryption of contents, names and manifest. |
| Someone on the network joins a transfer | Listener only while receiving; SPAKE2 with an 8-digit code (one online guess per connection, 5 attempts); the receiver must accept the named sender's offer; authenticated encryption afterwards. |
| Forgotten password | A recovery key is shown once after the build; the UI says plainly that losing both makes the snapshot unreadable. |
| Unsigned releases trigger SmartScreen/Gatekeeper warnings | The release workflow signs and notarizes when certificates are configured; docs/PACKAGING.md tells users what each prompt means. Snapshots copied from drives carry no download mark, so their restore programs run without prompts. |
| Release workflow untested until the first tag | Its scripts are the same ones run locally (`build-release.sh` produced and install-tested the Linux packages); the YAML is parsed in review only. |
| Drives that cannot hard-link (exFAT, FAT32) | Updates then copy unchanged files from the earlier snapshot instead of linking them; still complete and independent. |
| mDNS blocked (guest Wi-Fi, firewalls) | The receiver also shows its address and port for typing in; discovery is a convenience, pairing is what secures it. |
| Hostile or corrupted snapshots | `deny_unknown_fields`, path validation, no commands in the manifest, checksum check on every restored file, no writing through symlinks. |
| Cross-filesystem names (exFAT/NTFS) | Stored names are made portable (`CON` → `_CON`); originals are kept in the manifest and restored where valid. |
| Online installs change over time (new versions, removed packages) | Exact IDs, a vetted registry (`scripts/check-registry.mjs` checks all 519 IDs against the live catalogs), fallbacks in order, and a report of what was used; the recorded release tag is tried before the latest |
| Package manager prompts interrupt an unattended restore | One elevated batch per manager, asked first; CLI warms `sudo` before starting |
| Source builds run project code | Separate, visible plan items that can be unticked or turned off; pinned commit; user-level install |
| GitHub API rate limit (60/hour without a token) | Short response cache, `GITHUB_TOKEN` support, a message with the reset time; package managers are preferred over GitHub where available |
| A broken update | Verified against two checksums when published; AppImage/app swaps keep the old copy until the new one starts; packages and Windows installers roll back themselves |
| Glibc-linked engine on older distros | `scripts/build-restore-kit.sh` builds static musl engines (x86_64, aarch64) with rust-lld; the CLI and desktop app prefer them. |
| Installer bundles are large and slow | Flatpak runtimes are bundled once per snapshot and shared; preflight shows an estimate; collection reports progress and can be cancelled and resumed. RPM sets are the full dependency closure (weak deps off) so a fresh install of the same release works offline. |
| Distro packages on a different release | RPM recipes record `ID VERSION_ID`; restore offers them only on the same release, otherwise manual instructions. |
| Same processor, different distribution (Fedora RPMs on Linux Mint or openSUSE) | `compat::bundle_fits` decides from the destination profile's formats and release, never from the processor; package names are looked up per distribution family; nothing is converted. Tested by `tests/compat_matrix.rs` for every pair in the brief. |
| A distribution Simple Setup does not know | `ID_LIKE` picks the closest profile; otherwise the generic profile (portable formats only). Users add profiles (a parent and an os-release ID suffice); detection can be overridden in Settings. |
| Hostile or broken profile files | Size limit, strict schemas, YAML alias limits, validation of every field and chain; commands only as trusted, shell-free argument lists; official updates checksum-verified and command-free. |
| Packages from repositories the distribution does not vouch for | Origins come from the package manager (APT's signed release Origin, DNF repo IDs, …) and are matched against the profile; others are tried last and need approval. |
| Profile data goes stale (new releases, renamed repositories) | Profiles are versioned and updatable without an app release; unknown repositories only ask for approval, they never install silently. |

## Milestones

- [x] **M1** Skeleton, manifest schema, Linux discovery adapters with tests, Applications inventory, interactive selection → valid manifest.
- [x] **M2** External/local snapshot creation, checksums, verification, progress, resume, Snapshots management, restore preview and execution, end-to-end round trip (core test + GUI via WebDriver + offline launcher).
- [x] **M3** Offline installers (Linux): Flatpak bundles (app + runtime + installed GL/codec extensions) → `flatpak install --user --bundle`; `dnf download --resolve --alldeps` → `dnf install --cacheonly --disablerepo=*` of the packages the target lacks; `snap download` (+ base and content providers) → `snap ack` + `snap install`; AppImage / self-contained binary copy. Typed recipes, checksum before use, sudo/pkexec only for package managers, post-install verification. Static musl engines. Deferred: downloading GitHub release assets (M5 recipes), DPKG/Pacman collection, snap restore testing (no snapd in containers).
- [x] **M4** Windows adapters: installed programs from the registry uninstall keys (64-bit, 32-bit, per-user; runtimes/updates hidden), Microsoft Store/MSIX (`Get-AppxPackage`, state under `Packages\<family>`), Scoop (`persist` as data), Chocolatey (`lib/*.nuspec`). macOS adapters: `.app` bundles in `/Applications` and `~/Applications` (XML or binary Info.plist, App Store receipt, `~/Library/Preferences|Application Support|Containers/<bundle id>`), Homebrew formulae and casks (`brew info --json=v2 --installed`, auto-update and analytics off), MacPorts, installer receipts. Registry gained Windows/macOS settings paths, bundle IDs and installed-app names; secret/cache rules for Keychains, Windows credentials/DPAPI keys, caches and temp. Windows/macOS reinstalls are manual instructions (exact `scoop`/`choco`/`brew`/`winget` command where known). Deferred: winget is not run (it may refresh its catalog and send telemetry; the registry supplies winget IDs); offline Windows/macOS installers (M5).
- [x] **M5** Cross-OS installers: for Windows, macOS and other-architecture targets the build downloads the installer from a vetted catalog (winget community manifests via the registry's winget ID, Homebrew cask API, else the app's latest GitHub release; or a link + SHA-256 the user registered), verifies the published SHA-256, and records typed `windows` / `mac_app` recipes. Restore runs MSI/NSIS/Inno/WiX-bundle installers with fixed switches (consent prompt via RunAs only when Windows asks for it), copies `.app` bundles out of disk images/zips, and confirms the result in the installed-programs list or by bundle ID; other installer types are bundled for the user to open. Settings adapters (VS Code, Git, Firefox, Thunderbird, GitHub CLI, Neovim) move settings between operating systems, rewrite `.gitconfig`/`profiles.ini`, and keep machine-specific files in the snapshot. GitHub provenance now also comes from AppStream metadata. Registry IDs were checked against the live catalogs. Deferred: source builds from GitHub (needs a sandboxed, user-approved build design), DPKG/Pacman package collection, Homebrew formulae (bottles) and Windows ARM64 engines.
- [x] **M6** Paired LAN transfer: receive mode with mDNS announcement and an 8-digit code, SPAKE2 pairing, ChaCha20-Poly1305 frames, receiver accepts the named offer, per-file SHA-256 on arrival, resume from partial files, hand-off to the restore preview (CLI `send`/`receive`/`peers`; GUI Send dialog on Snapshots and after a build, Receive dialog on Restore). Optional encryption with age: random X25519 key sealed with the password, recovery key shown once, encrypted manifest, reports and keyed-hash file names, verification without the password. Incremental updates: hard links (or copies) of unchanged files and reuse of installers for unchanged app versions; an update stays encrypted with the same key. Deferred: sending straight from the build without a local copy, encrypting installer files, transfers over the internet (LAN only), changing the password of an existing snapshot.
- [x] **M7** Release packages: every desktop package (deb, rpm, AppImage, MSI/NSIS, DMG/app) carries the CLI and a restore kit with restore programs for Linux, Windows and macOS on x86-64 and ARM64 (`scripts/build-release.sh`, `.github/workflows/release.yml` with optional Developer ID signing/notarization and Windows signing, one `SHA256SUMS`); `build::kit_dirs` finds the kit in every package layout. A double-clickable `restore-windows.cmd` launcher. CI on x86-64 and ARM64 runners for all three OS families, plus a GUI job running the WebDriver accessibility audit and acceptance flow (`e2e/`). Accessibility audit (docs/ACCESSIBILITY.md): axe-core WCAG 2.2 AA on 17 states × 2 themes, keyboard, reflow and zoom; found and fixed target size, light-theme contrast, focus return, skip link, page titles, zoom, reduced motion. Packaging, signing and OS-prompt documentation (docs/PACKAGING.md). Not done: running the release workflow (no tag pushed), signing with real certificates, screen-reader testing.
- [x] **M8** Restore anywhere and stay up to date: restore installs apps the snapshot has no installer for with the new computer's own package managers or verified downloads (registry grown to 100 apps and 41 GitHub repositories, all IDs checked live), GitHub release archives and recognised `.exe` installers, source builds of checkouts at their commit with a toolchain installed first, batched elevation, resumable downloads usable offline, reports that record what was used and survive cancellation, Windows restart detection; `restore --offline/--no-source-builds`, `receive --restore`. Self-update from GitHub releases. Settings page, themes, new icon, dashboard and restore preview redesign. CI: the gui job fixed (dash `kill --`), packages built and checked on every PR, releases published on tags. Verified: Fedora 44, Ubuntu 24.04 and Wine containers (see README). Not run: macOS installs, real Windows (UAC, winget), Flathub and snap installs, the first published release and an update from it.
- [x] **M9** Distribution-aware compatibility and guided migration (0.4.0): distribution profiles
  with inheritance, os-release detection with `ID_LIKE` fallback and user override, user/official
  layers, JSON/YAML import with validation and command trust, official updates from releases; a
  compatibility engine (`compat.rs`) for every install decision with repository provenance, rules,
  preferences and explanations; apk, rpm-ostree, transactional-update and trusted custom adapters;
  Alpine discovery; openSUSE and Alpine package names in the registry; snapshot format 2 (source
  distribution, destination) with migration of version 1; destination probe over paired
  transfers; CLI `system`, `distros`, `analyze`, `probe`, `--for`; the UI reorganised into
  Dashboard / Snapshots / Transfers / Applications / Settings with the five-step Create/Transfer
  flow, compatibility summaries, approvals, migration summary with export, transfer history and the
  distribution manager. Not done: offline APT/pacman/apk package sets; vendor `.rpm` downloads;
  installs on real image-based systems; profile adapters for package updates or removal (nothing
  in Simple Setup updates or removes packages).
