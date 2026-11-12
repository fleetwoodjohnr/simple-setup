#!/bin/sh
# Checks the registry's openSUSE ("suse") and Alpine ("apk") package names against the real
# repositories, inside throwaway containers (podman or docker; needs internet):
#   ./scripts/check-distro-names.sh
# Each name must be a package of exactly that name (not just something that provides it).
set -eu
cd "$(dirname "$0")/.."
run=$(command -v podman || command -v docker) || { echo "needs podman or docker" >&2; exit 2; }
names() {
  node -e '
    const r = require("./crates/core/registry/apps.json");
    const s = new Set(r.apps.flatMap((a) => a.packages[process.argv[1]] ?? []));
    console.log([...s].join("\n"));' "$1"
}
fail=0
echo "openSUSE Tumbleweed:"
missing=$(names suse | "$run" run --rm -i docker.io/opensuse/tumbleweed:latest sh -c '
  zypper --non-interactive --gpg-auto-import-keys refresh >/dev/null 2>&1
  while read -r n; do
    zypper --non-interactive --no-refresh search --match-exact -t package "$n" >/dev/null 2>&1 || echo "$n"
  done')
[ -z "$missing" ] && echo "  all found" || { echo "$missing" | sed "s/^/  FAIL: /"; fail=1; }
echo "Alpine (latest stable):"
missing=$(names apk | "$run" run --rm -i docker.io/library/alpine:latest sh -c '
  apk update >/dev/null 2>&1
  while read -r n; do
    apk search -e "$n" 2>/dev/null | grep -q "^$n-[^-]*-r[0-9]*$" || echo "$n"
  done')
[ -z "$missing" ] && echo "  all found" || { echo "$missing" | sed "s/^/  FAIL: /"; fail=1; }
exit $fail
