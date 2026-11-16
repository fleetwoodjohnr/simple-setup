#!/bin/sh
# Performance baseline on this computer, with generated data only (no personal files):
# scan, snapshot build of 20,000 small files plus one 2 GB file, verification, restore into a
# fresh home, and a loopback network transfer of the same snapshot. Prints one line per step.
#   cargo build --release -p simple-setup-cli && scripts/bench.sh [work folder]
set -eu
cd "$(dirname "$0")/.."
cli="${CLI:-$PWD/target/release/simple-setup}"
work="${1:-$PWD/target/bench}"
rm -rf "$work"; mkdir -p "$work/home/Documents/many" "$work/out" "$work/inbox" "$work/restored"
export SIMPLE_SETUP_NO_LOG=1 XDG_CONFIG_HOME="$work/config"
now() { date +%s.%N; }
took() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.1f", b - a }'; }

python3 - "$work/home/Documents/many" <<'PY'
import os, sys
d = sys.argv[1]
for i in range(20000):
    sub = os.path.join(d, f"folder{i // 200:03d}")
    os.makedirs(sub, exist_ok=True)
    with open(os.path.join(sub, f"file{i}.txt"), "wb") as f:
        f.write(os.urandom(512 + (i * 37) % 7680))
PY
dd if=/dev/urandom of="$work/home/Documents/big.bin" bs=4M count=512 status=none
bytes=$(du -sb "$work/home/Documents" | cut -f1)
mb=$((bytes / 1000000))
platform=$("$cli" system --json 2>/dev/null | python3 -c 'import json,sys; p=json.load(sys.stdin)["device"]["platform"]; print(p["os"]+"-"+p["arch"])')
printf '{"apps":[],"folders":[{"path":"%s","known":"Documents","exclude":[]}],"targets":[{"os":"%s","arch":"%s"}]}' \
  "$work/home/Documents" "${platform%%-*}" "${platform#*-}" > "$work/selection.json"

t=$(now); "$cli" --version >/dev/null; echo "start (--version): $(took "$t") s"
t=$(now); "$cli" scan --json > "$work/scan.json"; echo "scan: $(took "$t") s, $(python3 -c 'import json,sys;print(len(json.load(open(sys.argv[1]))["apps"]))' "$work/scan.json") apps"
t=$(now); HOME="$work/home" "$cli" snapshot create --selection "$work/selection.json" --dest "$work/out" --name bench --yes >/dev/null 2>&1
s=$(took "$t"); echo "build: $s s for $mb MB, 20,001 files ($(awk -v m="$mb" -v s="$s" 'BEGIN{printf "%.0f", m/s}') MB/s)"
t=$(now); "$cli" snapshot verify "$work/out/bench" >/dev/null 2>&1; echo "verify: $(took "$t") s"
t=$(now); HOME="$work/restored" "$cli" restore "$work/out/bench" --yes --offline >/dev/null 2>&1; echo "restore: $(took "$t") s"

"$cli" receive --dest "$work/inbox" --yes --no-announce > "$work/receive.log" 2>&1 &
rx=$!
while ! grep -q 'Address' "$work/receive.log" 2>/dev/null; do sleep 0.2; done
code=$(grep -Eo '[0-9]{4}-[0-9]{4}' "$work/receive.log" | head -1)
port=$(grep -Eo 'Address.*:([0-9]+)' "$work/receive.log" | grep -Eo '[0-9]+$')
t=$(now); "$cli" send "$work/out/bench" --to "127.0.0.1:$port" --code "$code" --yes >/dev/null 2>&1
s=$(took "$t"); wait "$rx"; echo "transfer (loopback): $s s ($(awk -v m="$mb" -v s="$s" 'BEGIN{printf "%.0f", m/s}') MB/s)"
rm -rf "$work"
