#!/bin/sh
# Builds a release for this computer's operating system into release/simple-setup-<version>/:
#   - the desktop app packages (deb/rpm/AppImage, msi/exe, dmg/app), each carrying the CLI and a
#     restore kit with restore programs for every platform in the kit
#   - the CLI for every platform in the kit, and the kit itself as an archive
#   - SHA256SUMS of all of the above
# The kit is target/release/restore-kit, built by scripts/build-restore-kit.sh for the platforms
# this computer can build; set KIT=<folder> to use a complete kit assembled elsewhere (CI does).
# TARGET=<rust target triple> builds for another architecture of this OS (e.g. x86_64-apple-darwin
# on an Apple-silicon Mac); BUNDLES=<list> limits the package formats (e.g. nsis).
# Signing: see docs/PACKAGING.md (the Tauri signing environment variables are passed through).
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
kit=target/release/restore-kit
if [ -n "${KIT:-}" ]; then
  rm -rf "$kit" && mkdir -p "$kit" && cp -R "$KIT"/. "$kit"/
else
  ./scripts/build-restore-kit.sh
fi
triple=${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}
rel=target/$triple/release
cargo build --release -p simple-setup-cli --target "$triple"
mkdir -p src-tauri/binaries
case $triple in
  # The static Linux build runs on any distribution.
  *linux*) cp "$kit/linux-${triple%%-*}/simple-setup" "src-tauri/binaries/simple-setup-$triple" ;;
  *windows*) cp "$rel/simple-setup.exe" "src-tauri/binaries/simple-setup-$triple.exe" ;;
  *) cp "$rel/simple-setup" "src-tauri/binaries/simple-setup-$triple" ;;
esac
npx tauri build --target "$triple" --config src-tauri/tauri.release.conf.json ${BUNDLES:+--bundles "$BUNDLES"}

out=release/simple-setup-$version
rm -rf "$out" && mkdir -p "$out"
for f in "$rel"/bundle/*/*; do
  case $f in
    *.deb|*.rpm|*.AppImage|*.msi|*.exe|*.dmg) cp "$f" "$out/" ;;
    *.app) (cd "$(dirname "$f")" && tar czf - "$(basename "$f")") > "$out/Simple-Setup-$version-macos-app.tar.gz" ;;
  esac
done
for d in "$kit"/*/; do
  p=$(basename "$d")
  for b in "$d"simple-setup "$d"simple-setup.exe; do
    [ -f "$b" ] || continue
    case $b in *.exe) cp "$b" "$out/simple-setup-$version-$p.exe" ;; *) cp "$b" "$out/simple-setup-$version-$p" ;; esac
  done
done
(cd target/release && tar czf - restore-kit) > "$out/restore-kit-$version.tar.gz"
cd "$out"
if command -v sha256sum >/dev/null; then sha256sum -- * > SHA256SUMS; else shasum -a 256 -- * > SHA256SUMS; fi
cat SHA256SUMS
