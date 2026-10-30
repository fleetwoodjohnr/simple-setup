# Snapshot format (schema version 1)

```
<Snapshot name>/
  simple-setup.json        manifest (this document)
  README.txt               plain-language restore instructions
  launchers/               restore-linux.sh, restore-macos.command, restore-windows.ps1 (selected targets only)
  restore-engine/<os>-<arch>/simple-setup[.exe]
  installers/flatpak/<name>-<branch>-<arch>.flatpak   Flatpak app and runtime bundles (runtimes shared)
  installers/<source>-<App>/rpms/*.rpm                RPM package plus its full dependency set
  installers/<source>-<App>/snaps/<name>.{snap,assert}
  installers/<source>-<App>/<program file>            AppImage or self-contained binary
  installers/<os>-<arch>/<App>/<App>-<version>.<ext>  installer downloaded for another platform
                                                      (.msi/.exe/.msix, .dmg/.zip/.pkg, .AppImage)
  app-configurations/<App>/<location>/…   app settings
  app-data/<App>/<location>/…             app data/profiles
  personal-files/<Folder>/…               personal folders and files
  checksums/SHA256SUMS     `sha256sum -c` compatible list of every file
  reports/build-report.{json,txt}
  -- encrypted snapshots only --
  keys/snapshot-key.age    the snapshot key, encrypted with the password (age, scrypt)
  simple-setup.json.age    the full manifest, encrypted to the snapshot key
  data/<xx>/<keyed hash>   every copied file, encrypted (replaces the three folders above)
  reports/build-report.{json,txt}.age
```

`simple-setup schema` prints the authoritative JSON Schema. Key fields:

| Field | Meaning |
|---|---|
| `schema_version` | `1`. Readers refuse other versions. |
| `id`, `name`, `created_at`, `generator`, `source` | Identity, RFC 3339 UTC time, writer version, source device (`hostname`, `os_name`, `platform`). |
| `targets` | Selected target platforms (`{os, arch}`). Engines/launchers are only included for these. |
| `selection` | Exactly what the user chose (apps with installer/config/data flags and excludes, folders, opted-in secrets, targets). Used to show and reuse the choice. |
| `apps[]` | Identity and provenance of each selected app: source, native ID, canonical registry ID, version, GitHub info with confidence (and, for source checkouts, optional `commit`: the full hash, and `build`: `{tool: cargo|go|pipx, name}` read from `Cargo.toml`/`go.mod`, so a restore can rebuild exactly that code), evidence, chosen parts. `source` is one of `flatpak`, `rpm`, `dpkg`, `pacman`, `snap`, `app_image`, `custom`, `git_clone`, `manual` (Linux and any OS), `arp` (Windows installed-apps entry; native ID is the uninstall key name), `msix`, `scoop`, `choco` (Windows), `brew`, `brew_cask`, `app_bundle` (native ID is the bundle identifier), `pkg`, `macports` (macOS). Schema version 1 readers accept the added values; older builds reject a manifest that uses them. |
| `roots[]` | Each copied tree or file: `kind` (config/data/folder), owning `app`, `known` folder, `original` location (`~/…` or absolute), bundle `dir`. |
| `files[]` | `root`, original relative path `rel`, portable `stored` path, `size`, `sha256`, `mtime`, `mode`. |
| `installers[]` | Offline installer per app and `platform`: a typed `recipe` plus `evidence` (how the files were obtained and checked). Recipes: `flatpak` (`app_ref`, ordered `bundles`), `rpm` (`distro` such as `fedora 44`, requested `packages`, `rpms` with package names), `snap` (`classic`, ordered `snaps` with `.snap` and `.assert`), `program` (`original` under `~/`, one file), `windows` (`installer`: `msi`, `nullsoft`, `inno`, `burn`, or `exe`/`msix` for files the user runs; `product`: names expected in the installed-programs list), `mac_app` (`container`: `dmg`, `zip` or `pkg`; `app`: the `.app` inside, if known; `bundle_id`). Every file carries `stored`, `size`, `sha256`; `evidence` records the download URL, the catalog entry (winget manifest, Homebrew cask, GitHub release or the user's own link) and that the published SHA-256 matched. |
| `links[]` | Symlinks recorded (never followed); restored only if relative and inside their root. |
| `excluded` | Counts and examples of withheld secrets, caches, user exclusions, unreadable and special files. |
| `coverage[]` | Per app × target × part: `ready` / `manual` / `unsupported` with a reason. Settings restored on another operating system name the adapter used (see below). |
| `readiness[]` | Per target: `offline_ready` (only if everything is `ready` and an engine is bundled), reasons. |
| `warnings`, `total_bytes` | Build warnings (e.g. files that changed during copy) and content size. |
| `encryption` | Only for encrypted snapshots: `key_file`, `manifest` (the sealed full manifest) and `recipient` (public `age1…` key). The plain `simple-setup.json` then lists no apps, roots, files, installers or coverage, and its selection holds only the targets. |
| `base` | Only for incremental updates: the earlier snapshot's `id` and `name`, and how many files/bytes were taken from it instead of copied. |
| `files[].stored_sha256` | Only in encrypted snapshots: SHA-256 of the stored (encrypted) copy, which `SHA256SUMS` lists; `sha256` remains the original content's checksum, checked after decryption. |

Validation on read: unknown fields are rejected; `dir`/`stored` must be relative, `/`-separated,
free of `.`/`..`, drive letters, backslashes and Windows-reserved names; `rel` may not escape its
root; `original` must be `~/…` or absolute without `..`; checksums must be 64 hex digits.
Installer files must live under `installers/`; package, snap and Flatpak names must be plain
names (letters, digits, `._+-`, not starting with `-` or `.`), Flatpak refs must be
`app|runtime/<name>/<x86_64|aarch64>/<branch>` with the app bundle last, and program originals
must be under `~/` without `.`/`..`. Windows product names must be plain text; macOS app names
must be a single `*.app` name and bundle IDs plain names. The manifest contains no commands;
each recipe type maps to fixed argument lists in code (`flatpak install --user --noninteractive
--no-related --bundle`, `dnf install --cacheonly --disablerepo=* --skip-broken`, `snap ack` +
`snap install`, a `create_new` copy, `msiexec /i <file> /passive /norestart`, NSIS `/S`, Inno
`/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-`, WiX bundle `/quiet /norestart`, or `hdiutil
attach -readonly` / `ditto -x -k` followed by `ditto` into Applications). Installer switches
published by catalogs are never used.

**Settings on another operating system.** A settings or data root restored on a different OS goes
through the app's adapter in `crates/core/src/adapters.rs` (VS Code, Git, Firefox, Thunderbird,
GitHub CLI, Neovim). The adapter maps each file from the source OS folder to the target OS
folder (destinations come from code, not from the manifest), copies portable files, rewrites a
few (`.gitconfig`: credential helpers, `core.autocrlf` and absolute paths of the old system are
commented out; `profiles.ini`: `[Install…]` sections removed) and leaves files tied to the old
computer in the snapshot, reporting them. Roots of apps without an adapter are reported as
unsupported and stay in the snapshot.

**Encrypted snapshots.** Restore reads `encryption`, asks for the password or recovery key,
unlocks `key_file`, decrypts `manifest`, validates it like a plain manifest and requires the same
`id` and `encryption` block. Copied files are decrypted while they are restored and checked
against `sha256`. `simple-setup snapshot verify` and `sha256sum -c` work without the password.
The files are standard age files: `age -d keys/snapshot-key.age` (password) yields the
recovery key, and `age -d -i <file with that key>` opens the manifest and any copied file.

**Incremental updates.** An update is a complete, independent snapshot. Files whose size and
modification time match the earlier snapshot's record are hard-linked from it (copied if the
drive cannot hard-link), and installers are reused when the installed version is unchanged.
Deleting the earlier snapshot does not affect the update.

The `installers` field was added during M3 and defaults to empty, so earlier snapshots still read.
The `windows` and `mac_app` recipes were added in M5; earlier builds reject manifests that use them.
