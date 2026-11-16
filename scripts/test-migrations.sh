#!/bin/sh
# shellcheck disable=SC2034 # image variables are read through image()
# Real cross-distribution migrations in throwaway containers: each scenario installs apps with
# the source distribution's package manager, builds a snapshot there for the destination
# distribution, restores it in a fresh destination container, checks that the apps run, and
# restores again to check that nothing is done twice. Nothing is installed on this computer.
#
#   scripts/test-migrations.sh                 # every scenario
#   scripts/test-migrations.sh fedora-mint     # some of them
#   ENGINE=/path/to/static/simple-setup  RUNTIME=docker  scripts/test-migrations.sh
#
# Needs: podman (or docker), internet, and a static engine:
#   CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
#     cargo build --release -p simple-setup-cli --target x86_64-unknown-linux-musl
set -eu
cd "$(dirname "$0")/.."
engine="${ENGINE:-$PWD/target/x86_64-unknown-linux-musl/release/simple-setup}"
rt="${RUNTIME:-podman}"
[ -x "$engine" ] || { echo "No static engine at $engine (see the header of this script)." >&2; exit 2; }
out=$(mktemp -d "${TMPDIR:-/tmp}/simple-setup-migrations.XXXXXX")

fedora=registry.fedoraproject.org/fedora:44
mint=docker.io/linuxmintd/mint22-amd64:latest
ubuntu=docker.io/library/ubuntu:24.04
debian=docker.io/library/debian:13
arch=docker.io/library/archlinux:latest
suse=docker.io/opensuse/tumbleweed:latest
alpine=docker.io/library/alpine:3.22

# The Mint image ships Ubuntu's os-release; a real Mint 22 has this one.
mint_release='printf "%s\n" "NAME=\"Linux Mint\"" "VERSION=\"22 (Wilma)\"" "ID=linuxmint" "ID_LIKE=\"ubuntu debian\"" "PRETTY_NAME=\"Linux Mint 22\"" "VERSION_ID=\"22\"" "VERSION_CODENAME=wilma" "UBUNTU_CODENAME=noble" > /etc/os-release'

# How each package manager installs the scenario's apps on the source, and the scan source key.
install_cmd() {
  case "$1" in
    rpm) echo "dnf install -y -q $2" ;;
    # Images ship some apps as dependencies of their base set; a user's own install is manual.
    dpkg) echo "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $2 && apt-mark manual $2" ;;
    pacman) echo "pacman -Syu --noconfirm --needed $2" ;;
    apk) echo "apk add -q $2" ;;
  esac
}

# name | source image | source key | source prep | destination profile | destination image | destination prep | network
scenarios='
fedora-mint|fedora|rpm||linuxmint|mint|mint_release|online
fedora-mint-offline|fedora|rpm||linuxmint|mint|mint_release|none
mint-fedora|mint|dpkg|mint_release|fedora|fedora||online
ubuntu-arch|ubuntu|dpkg||arch|arch||online
debian-opensuse|debian|dpkg||opensuse-tumbleweed|suse||online
arch-fedora|arch|pacman||fedora|fedora||online
fedora-alpine|fedora|rpm||alpine|alpine||online
unknown-imported|fedora|rpm||linux|mint|nimbus|online
'
apps="jq git"

image() { eval "echo \$$1"; }
# shellcheck disable=SC2028 # the escapes are for the container's printf
prep() {
  case "$1" in
    mint_release) echo "$mint_release" ;;
    # A distribution Simple Setup has never heard of, recognised after importing a profile.
    nimbus) echo 'printf "%s\n" "ID=nimbus" "NAME=\"Nimbus OS\"" "VERSION_ID=1" > /etc/os-release; printf "%s\n" "schema_version: 1" "distros:" "  - id: nimbus" "    name: Nimbus OS" "    parent: ubuntu" "    match: {ids: [nimbus]}" > /tmp/nimbus.yaml; /engine distros validate /tmp/nimbus.yaml; /engine distros import /tmp/nimbus.yaml' ;;
    *) echo true ;;
  esac
}

failed=
want="${*:-}"
for line in $scenarios; do
  IFS='|' read -r name src key sprep dprofile dst dprep net <<EOF
$line
EOF
  if [ -n "$want" ] && ! echo " $want " | grep -q " $name "; then continue; fi
  echo "=== $name"
  dir="$out/$name"; mkdir -p "$dir"
  ids=$(for a in $apps; do printf '{"id":"%s:%s","installer":true,"config":false,"data":false},' "$key" "$a"; done)
  printf '{"apps":[%s],"folders":[],"targets":[{"os":"linux","arch":"x86_64"}]}' "${ids%,}" > "$dir/selection.json"
  if ! "$rt" run --rm -v "$engine:/engine:ro,z" -v "$dir:/out:z" "$(image "$src")" sh -ec "
      $(prep "$sprep"); $(install_cmd "$key" "$apps") >/dev/null
      /engine snapshot create --selection /out/selection.json --for $dprofile --dest /out --name snap --yes > /out/build.log 2>&1 || { cat /out/build.log; exit 1; }"
  then echo "FAIL $name: build"; failed="$failed $name"; continue; fi
  netflag=; [ "$net" = none ] && netflag="--network none"
  # shellcheck disable=SC2086 # netflag is one optional flag
  if "$rt" run --rm $netflag -v "$engine:/engine:ro,z" -v "$dir:/out:z" "$(image "$dst")" sh -ec "
      $(prep "$dprep")
      /engine system > /out/system.txt 2>&1 || true
      /engine restore /out/snap --yes > /out/first.log 2>&1 || true
      cat /out/first.log
      if [ '$net' = none ]; then
        grep -qi 'internet' /out/first.log
      else
        for a in $apps; do \"\$a\" --version >/dev/null; done
        /engine restore /out/snap --yes > /out/second.log 2>&1 || true
        cat /out/second.log
        for a in $apps; do grep -Eiq \"\$a.*(already installed|is installed)\" /out/second.log; done
        ! grep -qi 'Installed jq\.' /out/second.log
      fi"
  then echo "PASS $name"
  else echo "FAIL $name"; failed="$failed $name"; fi
done
echo "Logs: $out"
[ -z "$failed" ] || { echo "Failed:$failed"; exit 1; }
