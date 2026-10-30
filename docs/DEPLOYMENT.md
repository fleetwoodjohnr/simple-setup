# Moving your setup to another computer

Simple Setup works between any two of Linux, Windows and macOS, in either direction.

## 1. On the old computer: make a snapshot

1. **Create Snapshot.** Tick the apps to bring. For each app you can choose **Reinstall**,
   **Settings** and **Data**.
2. Add personal folders, such as Documents or project folders.
3. Choose where the snapshot goes: an external drive or any folder.
4. Choose **Restore on**: tick every kind of computer you might restore it on. For each one, the
   snapshot then carries:
   - installers, where they can be bundled (so restoring needs no internet);
   - a restore program for that system.
5. Optionally encrypt it with a password.
6. **Build.** The review step shows, for each target, what restores offline and what will need
   the internet.

## 2. Get the snapshot to the new computer

- **Drive:** plug it in on the new computer and open the snapshot folder.
- **Network:** on the new computer, choose **Restore → Receive from another computer**; it shows
  a pairing code. On the old computer, choose **Snapshots → Send…**, pick the new computer and
  type the code.
  - **Restore automatically when it arrives** restores everything as soon as the snapshot has
    been checked, without another click. This is the unattended path.
  - From a terminal: `simple-setup receive --restore --yes`.

## 3. On the new computer: restore

Open the snapshot in **Restore**. The preview is the dry run; nothing changes until you confirm.
It groups everything by how it will arrive:

| Group | What happens |
|---|---|
| Installs from the snapshot | Installers bundled when the snapshot was made. No internet needed. |
| Downloads and installs | Apps the snapshot has no installer for. See "How apps are installed" below. |
| Builds from source code | A GitHub project you had cloned, rebuilt at the exact commit you had, with Cargo, Go or pipx. The build tool is installed first if needed. Untick it to skip. |
| Settings and personal files | Copied into place. Settings for another operating system go through per-app converters (VS Code, Git, Firefox, Thunderbird, GitHub CLI, Neovim). Existing files are never overwritten. |
| Needs you | Store apps, installers without an unattended mode, or things that need internet while offline. Each comes with instructions. |
| Not possible on this computer | No version of the app exists for this system. It is skipped and reported. |

### How apps are installed

Apps the snapshot has no installer for are installed in this order, by whichever works first:

1. **The computer's own package managers.**
   - Flathub, which needs no password.
   - winget.
   - Homebrew.
   - APT, DNF, Zypper, Pacman or snap. These ask for the administrator password once for all
     apps together.
2. **A download**, checked against its published SHA-256, from:
   - the winget catalog;
   - Homebrew's cask list;
   - the app's GitHub release.

Before anything starts:
- the confirmation lists everything that needs internet or a password;
- the terminal version asks for the password up front.

While it runs:
- each item shows its progress;
- **Details** shows the full log;
- one item failing never stops the others.

The report says how each app was installed: method, release, file, checksum and commit. It is
saved in the settings folder under `reports/`. From the report you can:
- **Retry failed** to try the failed items again;
- run the restore again later, which finishes anything left and skips what is done (useful
  after a restart that a Windows installer asked for).

### Without internet

- Everything bundled in the snapshot still installs.
- Installers that an earlier restore downloaded are reused.
- Apps that would need a download are listed under **Needs you**; connect and restore again.
- To use only the snapshot, even when online: turn off **Settings → Restoring → Install apps the
  snapshot has no installer for**, or run `simple-setup restore <snapshot> --offline`.

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
