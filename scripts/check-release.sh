#!/bin/sh
# Checks what scripts/build-release.sh produced for this operating system: the expected packages
# exist, they contain the app icon, the CLI and the restore kit, SHA256SUMS verifies, and the
# restore program for this computer runs. Exits non-zero with every problem listed.
#   scripts/check-release.sh [release/simple-setup-<version>]
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
out=${1:-release/simple-setup-$version}
kit=target/release/restore-kit
problems=0
fail() { echo "FAIL: $*"; problems=$((problems + 1)); }
ok() { echo "ok:   $*"; }
first() { for f in "$@"; do [ -e "$f" ] && { echo "$f"; return; }; done; }
need() { [ -n "$2" ] || fail "no $1 in $out"; }
[ -d "$out" ] || { echo "FAIL: $out does not exist"; exit 1; }
root=$(pwd)
cd "$out"; here=$(pwd)
echo "Checking $(pwd)"

# Every file is listed in SHA256SUMS and matches.
if command -v sha256sum >/dev/null; then sum="sha256sum"; else sum="shasum -a 256"; fi
if $sum -c SHA256SUMS >/dev/null; then ok "SHA256SUMS verifies"; else fail "SHA256SUMS does not verify"; fi
for f in *; do
  # "<hash>  name", or "<hash> *name" where sha256sum runs in binary mode (Windows).
  [ "$f" = SHA256SUMS ] || grep -qF -e "  $f" -e " *$f" SHA256SUMS || fail "$f is not in SHA256SUMS"
done

# The CLI for every platform in the kit, and the kit archive.
for d in "$root/$kit"/*/; do
  p=$(basename "$d")
  [ -e "simple-setup-$version-$p" ] || [ -e "simple-setup-$version-$p.exe" ] || fail "no CLI for $p"
done
[ -e "restore-kit-$version.tar.gz" ] && ok "restore kit archive" || fail "no restore-kit-$version.tar.gz"
echo "      restore kit: $(cd "$root/$kit" && echo *)"

# Lists a package's files, one per line.
deb_list() {
  # Mode, owner, size, date and time come first; the path (which may contain spaces) is the rest.
  if command -v dpkg-deb >/dev/null; then dpkg-deb -c "$1" | sed -E 's/^([^ ]+ +){5}//; s|^\./|/|; s|^([^/])|/\1|'; else
    t=$(mktemp -d) && (cd "$t" && ar x "$here/$1" && tar tf data.tar.* | sed 's|^\./|/|; s|^\([^/]\)|/\1|') && rm -rf "$t"
  fi
}
has() { # has <listing> <description> <grep pattern>
  if printf '%s\n' "$1" | grep -q "$3"; then ok "$2"; else fail "$2 missing"; fi
}

case $(uname -s) in
  Linux)
    deb=$(first ./*.deb); rpm=$(first ./*.rpm); img=$(first ./*.AppImage)
    need .deb "$deb"; need .rpm "$rpm"; need .AppImage "$img"
    if [ -n "$deb" ]; then
      l=$(deb_list "$deb")
      has "$l" "deb: app" '/usr/bin/simple-setup-gui$'
      has "$l" "deb: CLI" '/usr/bin/simple-setup$'
      has "$l" "deb: icon" '/usr/share/icons/hicolor/.*/apps/simple-setup-gui.png$'
      has "$l" "deb: restore kit" '/usr/lib/Simple Setup/restore-kit/linux-'
    fi
    if [ -n "$rpm" ] && command -v rpm >/dev/null; then
      l=$(rpm -qlp "$rpm" 2>/dev/null)
      has "$l" "rpm: app" '/usr/bin/simple-setup-gui$'
      has "$l" "rpm: icon" '/usr/share/icons/hicolor/.*/apps/simple-setup-gui.png$'
      has "$l" "rpm: restore kit" '/usr/lib/Simple Setup/restore-kit/linux-'
    fi
    if [ -n "$img" ]; then
      [ "$(head -c 4 "$img" | od -An -c | tr -d ' ')" = "177ELF" ] && ok "AppImage is an executable" || fail "AppImage is not an ELF file"
    fi
    native="linux-$(uname -m | sed 's/arm64/aarch64/')/simple-setup" ;;
  Darwin)
    need .dmg "$(first ./*.dmg)"
    app=$(first ./*-macos-*-app.tar.gz); need "app archive" "$app"
    if [ -n "$app" ]; then
      l=$(tar tzf "$app")
      has "$l" "app: icon" 'Contents/Resources/icon.icns$'
      has "$l" "app: CLI" 'Contents/MacOS/simple-setup$'
      has "$l" "app: restore kit" 'Contents/Resources/restore-kit/macos-'
    fi
    native="macos-$(uname -m | sed 's/arm64/aarch64/')/simple-setup" ;;
  MINGW*|MSYS*|CYGWIN*)
    need "NSIS installer" "$(first ./*-setup.exe)"
    case ${BUNDLES:-} in nsis) ;; *) need "MSI installer" "$(first ./*.msi)" ;; esac
    a=$(uname -m); case $a in x86_64) a=x86_64 ;; *) a=aarch64 ;; esac
    native="windows-$a/simple-setup.exe" ;;
  *) fail "unknown OS $(uname -s)"; native= ;;
esac

# The restore program for this computer runs (it is what a snapshot carries).
if [ -n "$native" ] && [ -f "$root/$kit/$native" ]; then
  if v=$("$root/$kit/$native" --version); then ok "restore program runs: $v"; else fail "$native does not run"; fi
elif [ -n "$native" ]; then
  echo "note: no $native in the kit (cross-architecture build)"
fi

[ "$problems" -eq 0 ] || { echo "$problems problem(s)"; exit 1; }
echo "release looks complete"
