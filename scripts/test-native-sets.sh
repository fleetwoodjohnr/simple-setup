#!/bin/sh
# Real collection and disconnected restoration; all installs occur in disposable containers.
set -eu
cd "$(dirname "$0")/.."
engine="${ENGINE:-$PWD/target/x86_64-unknown-linux-musl/release/simple-setup}"
out=$(mktemp -d /tmp/simple-setup-native.XXXXXX)
for manager in ${*:-apt pacman apk}; do
  case "$manager" in
    apt) image=docker.io/library/ubuntu:24.04; source=dpkg; setup='apt-get update -qq && apt-get install -y -qq jq' ;;
    pacman) image=docker.io/library/archlinux:latest; source=pacman; setup='pacman -Syu --noconfirm jq' ;;
    apk) image=docker.io/library/alpine:3.22; source=apk; setup='apk add jq' ;;
    *) exit 2 ;;
  esac
  mkdir -p "$out/$manager"
  printf '{"apps":[{"id":"%s:jq","installer":true,"config":false,"data":false}],"folders":[],"targets":[{"os":"linux","arch":"x86_64"}]}' "$source" > "$out/$manager/selection.json"
  podman run --rm -v "$engine:/engine:ro,z" -v "$out/$manager:/out:z" "$image" sh -ec "$setup; /engine snapshot create --selection /out/selection.json --dest /out --name test --yes"
  # Must actually contain the requested dependency set; a successful build alone is insufficient.
  python3 - "$out/$manager/test/simple-setup.json" <<'PY'
import json,sys
m=json.load(open(sys.argv[1]))
assert any(i['recipe']['kind']=='native_set' and len(i['recipe']['packages']) > 1 for i in m['installers']), m['warnings']
PY
  podman run --rm --network none -v "$engine:/engine:ro,z" -v "$out/$manager:/out:z" "$image" sh -ec '/engine restore /out/test --offline --yes > /out/first-restore.log; cat /out/first-restore.log; grep -F "Installed jq." /out/first-restore.log; jq --version; /engine restore /out/test --offline --yes > /out/second-restore.log; cat /out/second-restore.log; grep -F "already installed" /out/second-restore.log'
done
echo "Native package set evidence: $out"
