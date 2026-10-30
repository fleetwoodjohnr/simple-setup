#!/bin/sh
# Runs a WebDriver script against the real desktop app in a virtual X display, with a throwaway
# home folder (the app scans this computer's installed apps read-only, but only reads and writes
# files under that home and the fixture folders).
#   e2e/run.sh a11y|acceptance
# Needs: tauri-driver (cargo install tauri-driver), WebKitWebDriver (webkit2gtk-driver /
# webkit2gtk4.1-devel), Xvfb, python3, and `npm ci`.
set -eu
cd "$(dirname "$0")/.."
which=${1:?a11y or acceptance}
fix=$(mktemp -d "${TMPDIR:-/tmp}/simple-setup-e2e.XXXXXX")
mkdir -p "$fix/home/Documents/notes" "$fix/home/.config" "$fix/usb" "$fix/inbox" "$fix/cli1" "$fix/cli2"
printf 'XDG_DOCUMENTS_DIR="$HOME/Documents"\n' > "$fix/home/.config/user-dirs.dirs"
echo "quarterly report" > "$fix/home/Documents/report.txt"
echo "- buy milk" > "$fix/home/Documents/notes/todo.md"
cargo build -q -p simple-setup-cli
npx tauri build --debug --no-bundle >/dev/null
# A private X display, and the driver in its own process group so that WebKitWebDriver and the
# app stop with it.
Xvfb -displayfd 3 -screen 0 1400x1000x24 -nolisten tcp 3> "$fix/display" 2>/dev/null &
xvfb=$!
driver=
trap 'kill -TERM -- "-$driver" 2>/dev/null; kill "$xvfb" 2>/dev/null; true' EXIT
while [ ! -s "$fix/display" ]; do sleep 0.2; done
env -u WAYLAND_DISPLAY DISPLAY=":$(cat "$fix/display")" HOME="$fix/home" GDK_BACKEND=x11 \
  WEBKIT_DISABLE_COMPOSITING_MODE=1 setsid tauri-driver --port 4444 > "$fix/driver.log" 2>&1 &
driver=$!
sleep 3
app=target/debug/simple-setup-gui
status=0
case $which in
  a11y) python3 e2e/a11y.py "$app" "$fix" node_modules/axe-core/axe.min.js "$fix/a11y-report.json" || status=$? ;;
  acceptance) python3 e2e/acceptance.py "$app" "$fix" target/debug/simple-setup || status=$? ;;
esac
echo "Screenshots and reports: $fix"
exit $status
