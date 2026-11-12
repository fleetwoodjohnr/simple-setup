# Moving your setup to another computer

Simple Setup works between any two of Linux, Windows and macOS, in either direction.

## 1. On the old computer: choose and check

From the dashboard choose **Create Snapshot** (save it to a drive or folder) or **Transfer to
Another Computer** (send it straight to a computer on this network). Both take five steps:

1. **Applications.** Tick the apps to bring. For each you can choose **Reinstall**, **Settings**
   and **Data**.
2. **Files.** Personal folders such as Documents or project folders. Keys, tokens and browser
   logins are left out unless you include them on purpose.
3. **Destination.**
   - *Transfer:* on the new computer open **Transfers → Receive on this computer**; it shows a
     pairing code. Enter it and choose **Connect**. Simple Setup asks that computer what it is —
     its system, distribution, processor, package managers — and prepares exactly what fits it.
   - *Snapshot:* **A computer like this one**, **Another kind of computer** (choose its system,
     Linux distribution and processor; several at once if you like), or **Not decided yet**
     (restore programs for every system come along and each app is matched when you restore).
     Then choose where to save it and, optionally, a password.
4. **Compatibility.** Each app is shown in one of four groups, with its installation plan:

   | Group | Meaning |
   |---|---|
   | Ready | Installs as before, or with the new computer's own equivalent (for example APT on Linux Mint for an app that was an RPM on Fedora). |
   | Another way | Installs differently than before, for example as a Flatpak or from the app's GitHub release. |
   | Needs you | An approval (a package from a repository the distribution doesn't vouch for, a source build, changing a read-only system) or internet that a snapshot-only restore won't have. |
   | Not available | No version for that computer is known; it is skipped and reported. |

   **Why?** next to an app lists every option that was considered and why it was or wasn't used.
   Package files are never converted (a Fedora RPM never goes to Linux Mint). The offline note
   says what will need the internet on the new computer.
5. **Start.** A snapshot is built and every copy checked. A transfer is built in a temporary
   folder, sent, checked file by file on arrival, and the temporary copy removed.

## 2. Getting a snapshot to the new computer

- **Drive:** plug it in on the new computer and open **Restore Snapshot**.
- **Network:** a transfer arrives by itself. To send a snapshot you already have, choose
  **Transfers → Send a snapshot** (or **Send…** on the Snapshots page).
  - **Restore automatically when it arrives** (on the receiving computer) restores everything
    as soon as the snapshot has been checked, without another click — except items that need
    your approval.
  - From a terminal: `simple-setup receive --restore --yes`.

## 3. On the new computer: restore

Open **Restore Snapshot**. The snapshot's integrity is checked, then the compatibility summary
appears again — this time checked against this computer's own package lists — followed by the
plan, which is the dry run. Nothing changes until you press **Start Restore**. The plan groups
everything by how it will arrive:

| Group | What happens |
|---|---|
| Installs from the snapshot | Installers bundled when the snapshot was made, if they fit this computer. No internet needed. |
| Downloads and installs | The distribution's own package, Flatpak, snap or a verified download. See "How apps are installed" below. |
| Needs your approval — tick to allow | Source builds, packages from repositories the distribution doesn't vouch for, custom package-manager commands, changes to a read-only system. They start unticked. |
| Settings and personal files | Copied into place. Settings for another operating system go through per-app converters (VS Code, Git, Firefox, Thunderbird, GitHub CLI, Neovim). Existing files are never overwritten. |
| Needs you | Store apps, installers without an unattended mode, or things that need internet while offline. Each comes with instructions. |
| Not possible on this computer | No version of the app exists for this system. It is skipped and reported. |

### How apps are installed

For each app, the first of these that fits this computer is used:

1. **The installer in the snapshot**, if it fits: RPM package sets only on the Fedora/RHEL
   release they were collected on, `.deb` files only on Debian-family systems, Flatpak bundles
   where Flatpak is available, AppImages where they run.
2. **The same kind as before**: a Flatpak stays a Flatpak, a snap stays a snap (their settings
   live inside their own folders).
3. **This distribution's own package** from its own repositories — APT, DNF, Zypper, pacman,
   apk; winget on Windows; Homebrew on macOS — under this distribution's package name (Firefox
   is `MozillaFirefox` on openSUSE). These ask for the administrator password once for all apps
   together.
4. **Flatpak from Flathub** (per user, no password), then **snap**.
5. **A download**, checked against its published SHA-256, from the winget catalog, Homebrew's
   cask list or the app's GitHub release.
6. **A source build** for GitHub projects you had cloned (with approval).

Settings → **Installation sources** can put Flatpak first, or turn snaps, downloads or source
builds off. Linux distributions are recognised from `/etc/os-release`; if yours is recognised
wrongly or not at all, see **Settings → This computer** and **Settings → Operating systems →
Linux distributions** ([DISTROS.md](DISTROS.md)).

Before anything starts:
- the confirmation lists everything that needs internet or a password;
- the terminal version asks for the password up front.

While it runs:
- each item shows its progress;
- **Details** shows the full log;
- one item failing never stops the others.

The report says how each app was installed: method, release, file, checksum and commit. It is
saved in the settings folder under `reports/`. From the migration summary you can:
- **Retry failed** to try the failed items again;
- **Export report** to save it as text or JSON;
- run the restore again later, which finishes anything left and skips what is done (useful
  after a restart that a Windows installer asked for).

### Without internet

- Everything bundled in the snapshot still installs.
- Installers that an earlier restore downloaded are reused.
- Apps that would need a download are listed under **Needs you**; connect and restore again.
- To use only the snapshot, even when online: turn off **Settings → Installation sources →
  Install apps with this computer's package managers and downloads**, or run
  `simple-setup restore <snapshot> --offline`.

### Without the desktop app

Every snapshot carries its own restore program; on a fresh computer you need nothing else:

| System | How to start it |
|---|---|
| Windows | Double-click `launchers\restore-windows.cmd`. |
| macOS | Double-click `launchers/restore-macos.command`. |
| Linux | Run `sh launchers/restore-linux.sh`. |

Add `--dry-run` to see the plan first, or `--yes` to skip the questions.

## Keeping Simple Setup up to date

**Settings → Updates** shows the installed version and the latest release on GitHub.

1. **Check for Updates.**
2. Read the release notes.
3. **Download Update.** The download resumes if it is interrupted, and is checked against the
   release's published checksums.
4. **Restart and Install.** Your settings and snapshots are kept.

The app can check once a day when it starts, and can download updates by itself to install at
the next restart.
