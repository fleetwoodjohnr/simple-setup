# Linux distributions

Simple Setup does not treat "Linux" as one system. Two computers with the same processor can
need different package formats, package managers and package names (Fedora uses RPM through
DNF, Linux Mint DEB through APT), and even two RPM distributions name packages differently
(Firefox is `firefox` on Fedora and `MozillaFirefox` on openSUSE). Each distribution is
therefore described by a **profile**, and every install decision is made from the destination's
profile and its live package lists — never from the source app's file type or the processor
alone.

## What a profile says

Profiles live in `crates/core/registry/distros.json` (built in), in official updates, and in the
user's own file. A profile names a `parent` and states only what differs; everything else is
inherited, ending in the generic `linux` profile.

```json
{ "schema_version": 1, "distros": [
  { "id": "nimbus", "name": "Nimbus OS", "parent": "ubuntu", "match": { "ids": ["nimbus"] } }
] }
```

That is a complete profile for a new Ubuntu-based distribution: it installs with APT, uses
Ubuntu's and Debian's package names and repositories, and inherits Ubuntu's rules.

| Field | Meaning |
|---|---|
| `id` | Lowercase ID. Built-in profiles use the os-release `ID`, so `selection.linux_distro` values of older snapshots still work. |
| `name` | Shown to people. |
| `parent` | The profile it inherits from (default `linux`). Cycles and more than 8 levels are refused. |
| `match` | `ids` (os-release `ID`s), optional `variants` (`VARIANT_ID`, e.g. `silverblue`), optional `min_version`/`max_version` (dotted `VERSION_ID`, inclusive). A profile with variants beats one without; a user profile beats a built-in one. |
| `family` | Shown grouping (`debian`, `fedora`, `suse`, `arch`…). |
| `arch` | Processors it is built for (`x86_64`, `aarch64`, `armv7`, `i686`, `riscv64`, `ppc64le`, `s390x`). |
| `package_manager` | `apt`, `dnf`, `zypper`, `pacman`, `apk`, `rpm-ostree`, `transactional-update`, `none` or `custom`. Each built-in adapter is a fixed set of command lines in `crates/core/src/resolve.rs`. |
| `formats` | `deb`, `rpm`, `pkg.tar.zst`, `apk`: the package files it can install. |
| `names` | Keys of the app registry (`registry/apps.json`) holding its package names (`dpkg`, `rpm`, `suse`, `pacman`, `apk`). The IDs of the profile chain are looked up first, so the registry may hold names for one distribution alone (`"linuxmint": [...]`). |
| `repositories` | Globs for the repositories that count as the distribution's own: the DNF repo ID, the signed APT release `Origin` (so mirrors still match), the Zypper alias or name, the pacman repository, the apk repository URL. A package found only elsewhere needs the user's approval. A child's repositories are added to its parent's. |
| `universal` | `flatpak`, `snap`, `appimage`: `default` (set up out of the box), `available`, or `unsupported` (e.g. snap on Linux Mint, AppImages on musl systems). |
| `immutable`, `rolling` | Image-based system (packages are layered and need a restart; bundled package files are not used); rolling release. |
| `rules` | `{ app, action: prefer|avoid|skip, method: native|flatpak|snap|download|source, reason }` for one registry app or `*`. A child's rule for an app replaces its parent's rules for that app (Ubuntu avoids its snap-only `firefox` package; Linux Mint, which packages Firefox itself, prefers it). |
| `custom` | For `package_manager: custom`: `label`, `install`, `query`, optional `available` argument lists, `admin`. See *Trust* below. |
| `notes` | One line of explanation, shown with the profile. |

## How a computer is recognised

`/etc/os-release` (or `/usr/lib/os-release`) is read: `ID`, `VARIANT_ID` and `VERSION_ID` pick a
profile; otherwise the first `ID_LIKE` entry that names one is used ("treated like Ubuntu");
otherwise the generic profile, which assumes only portable formats. `/run/ostree-booted` or
`transactional-update` mark the system read-only whatever the profile says. The kernel version is
recorded for diagnostics and never used for decisions. When detection is wrong, Settings → This
computer → *Treat this computer as* (stored as `distro_override`) picks a profile; `simple-setup
system` shows the result and how it was reached.

## How a decision is made

For each app (`crates/core/src/compat.rs`):

1. The snapshot's own installer, only if `bundle_fits` the destination: an RPM dependency set
   only on the release it was collected on and never on a read-only system; a vendor `.deb` only
   where `formats` contains `deb`; a Flatpak or snap only where that format is not unsupported; an
   AppImage only where AppImages run. Package files are never converted.
2. Otherwise the ways the destination can install it, best first: its own packaging kind first
   (a Flatpak stays a Flatpak, because its settings live in its sandbox folders), then the
   distribution's own package from its official repositories, Flatpak, snap, a verified download
   (GitHub release, winget, Homebrew cask), a source build. The profile's rules and the user's
   preferences (prefer Flatpak, allow snaps, downloads, source builds) reorder or remove options.
3. When the destination is this computer, its package manager is asked once which of the names
   it offers and from which repository. Names it does not offer are dropped; names from a
   repository the profile does not list need approval and are tried last.
4. Every option dropped carries a sentence saying why; the plan, the report and `simple-setup
   analyze` show them.

An app's own package name is reused only on a distribution that shares its package-name space
(`namespaces`): a Fedora RPM's name is tried on RHEL, never on openSUSE, Debian or Arch.

The result falls into one of four groups: **ready** (as before, or with the destination's own
equivalent), **alternative** (another kind, e.g. a Flatpak instead of an RPM), **review** (an
approval, a download while offline, or a manual step), **unavailable** (skipped, with the reason).

## Managing profiles

In the app: Settings → Operating systems → Linux distributions → *Manage distributions*. From
the command line:

```sh
simple-setup distros list                  # every profile, where it comes from, whether it is used
simple-setup distros show linuxmint        # one profile with its chain applied
simple-setup distros validate my.yaml      # check a file and test it against this computer
simple-setup distros import my.yaml        # add or replace your profiles (JSON or YAML)
simple-setup distros export nimbus --out nimbus.json
simple-setup distros disable pop           # built-in profiles can be switched off
simple-setup distros remove nimbus         # your own can be removed
simple-setup distros trust void            # trust a profile's custom commands (shown first)
simple-setup distros update                # newer official profiles from the GitHub release
simple-setup analyze SNAPSHOT --as linuxmint:22   # what would happen on such a computer
```

User profiles are saved in `~/.config/simple-setup/distros/user.json`, official updates in
`distros/official.json` next to it.

## Trust and safety

Imported profiles are untrusted data:

- Files over 256 KB are refused; JSON and YAML are read with strict schemas (unknown fields are
  errors) and YAML with a parser that limits aliases and nesting.
- IDs, names, versions, globs and rules are checked; a parent must exist and chains may not loop.
- No profile carries commands, except a user profile's `custom` adapter: argument lists, never a
  shell. The program must be a plain name or an absolute path, and may not be a shell, an
  interpreter or an elevation tool (`sh`, `python`, `sudo`, `pkexec`, `env`…); Simple Setup adds
  `sudo`/`pkexec` itself when `admin` is set. `{packages}` and `{package}` may only stand alone.
- Custom commands run only after the user trusts that exact set. The trust is a SHA-256 of the
  profile ID and its commands; changing a command, or re-importing different ones, drops it.
  Each use is shown in the restore plan, which leaves the item unticked until approved.
- Official updates come from the `distros.json` asset of this project's latest GitHub release,
  checked against GitHub's digest and the release's `SHA256SUMS` like app updates. They must be
  newer than the profiles in use and may not contain custom commands.
- A destination described over the network (Transfer flow) is used only for planning; any custom
  commands in it are dropped and never run on the sending computer.

## Adding a distribution to Simple Setup itself

Add a profile to `crates/core/registry/distros.json` (bump `version`), with `repositories`
checked on a real installation (`apt-cache policy`, `dnf repolist`, `zypper repos`…). If its
package names differ from its parent's, add a key to the affected apps in
`crates/core/registry/apps.json` and check them with `scripts/check-distro-names.sh` (openSUSE and
Alpine names) or `scripts/check-registry.mjs`. `cargo test -p simple-setup-core distros` validates
every built-in profile; add a row to `crates/core/tests/compat_matrix.rs` for the new destination.
