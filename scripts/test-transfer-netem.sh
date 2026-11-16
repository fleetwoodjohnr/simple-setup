#!/bin/sh
# Network transfer over a bad link, between two throwaway containers: 60 ms delay with jitter,
# 1 % packet loss and a 40 Mbit/s cap on the sender's side, and the connection killed in the
# middle (as a dropped Wi-Fi or a sleeping laptop would). The sender must reconnect by itself
# and finish; the snapshot must verify on the receiving side; a wrong pairing code must fail.
#
#   scripts/test-transfer-netem.sh
#   ENGINE=/path/to/static/simple-setup  RUNTIME=docker  scripts/test-transfer-netem.sh
#
# Needs podman (or docker), internet (to install iproute2 in the containers) and the static
# engine (see scripts/test-migrations.sh).
set -eu
cd "$(dirname "$0")/.."
engine="${ENGINE:-$PWD/target/x86_64-unknown-linux-musl/release/simple-setup}"
rt="${RUNTIME:-podman}"
image=docker.io/library/alpine:3.22
[ -x "$engine" ] || { echo "No static engine at $engine." >&2; exit 2; }
out=$(mktemp -d "${TMPDIR:-/tmp}/simple-setup-netem.XXXXXX")
net=simple-setup-netem-$$
cleanup() {
  "$rt" rm -f "$net-recv" "$net-send" >/dev/null 2>&1 || true
  "$rt" network rm "$net" >/dev/null 2>&1 || true
}
trap cleanup EXIT
"$rt" network create "$net" >/dev/null
mkdir -p "$out/in"
# tc and ss need netlink, which SELinux labels deny to containers.
run() { "$rt" run -d --name "$1" --network "$net" --cap-add NET_ADMIN --security-opt label=disable -v "$engine:/engine:ro,z" -v "$out:/out:z" "$image" sleep 600 >/dev/null; }
run "$net-recv"
run "$net-send"
for c in recv send; do "$rt" exec "$net-$c" apk add -q iproute2 iproute2-tc iproute2-ss >/dev/null; done

# 40 MB of incompressible data in a snapshot, made on the sender.
"$rt" exec "$net-send" sh -ec '
  mkdir -p /data && head -c 40000000 /dev/urandom > /data/random.bin && echo note > /data/small.txt
  printf "%s" "{\"apps\":[],\"folders\":[{\"path\":\"/data\",\"known\":null,\"exclude\":[]}],\"targets\":[{\"os\":\"linux\",\"arch\":\"x86_64\"}]}" > /tmp/sel.json
  /engine snapshot create --selection /tmp/sel.json --dest /tmp/snaps --name netem --yes >/dev/null'

"$rt" exec -d "$net-recv" sh -c '/engine receive --dest /out/in --yes --no-announce --port 41234 > /out/recv.log 2>&1'
code=
for _ in $(seq 1 50); do
  code=$(grep -Eo '[0-9]{4}-[0-9]{4}' "$out/recv.log" 2>/dev/null | head -1) && [ -n "$code" ] && break
  sleep 0.2
done
[ -n "$code" ] || { echo "FAIL: the receiver showed no code"; cat "$out/recv.log"; exit 1; }
send_ip=$("$rt" inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$net-send")

failed=
# A wrong code is refused (and does not stop the receiver).
wrong=$(echo "$code" | tr '0123456789' '1234567890')
if "$rt" exec "$net-send" /engine send /tmp/snaps/netem --to "$net-recv:41234" --code "$wrong" --yes > "$out/wrong.log" 2>&1; then
  echo "FAIL: a wrong code was accepted"; failed=1
elif grep -qi 'pairing code' "$out/wrong.log"; then echo "PASS wrong code refused"
else echo "FAIL: unexpected error for a wrong code"; cat "$out/wrong.log"; failed=1; fi

"$rt" exec "$net-send" tc qdisc add dev eth0 root netem delay 60ms 20ms loss 1% rate 40mbit
echo "Sender's link: $("$rt" exec "$net-send" tc qdisc show dev eth0)"
"$rt" exec -d "$net-send" sh -c "/engine send /tmp/snaps/netem --to $net-recv:41234 --code $code --yes > /out/send.log 2>&1; echo \$? > /out/send.exit"
# Kill the connection once a few MB have arrived.
for _ in $(seq 1 100); do
  [ "$(du -sk "$out/in" | cut -f1)" -gt 4000 ] && break
  sleep 0.2
done
"$rt" exec "$net-recv" ss -K dst "$send_ip" >/dev/null 2>&1 || true
echo "Connection killed after $(du -sh "$out/in" | cut -f1) had arrived."
for _ in $(seq 1 300); do [ -f "$out/send.exit" ] && break; sleep 1; done
if [ "$(cat "$out/send.exit" 2>/dev/null)" = 0 ] && grep -q 'Reconnecting' "$out/send.log"; then
  echo "PASS reconnected and finished"
else
  echo "FAIL: the send did not reconnect and finish"; cat "$out/send.log"; failed=1
fi
if "$rt" exec "$net-recv" /engine snapshot verify /out/in/netem > "$out/verify.log" 2>&1; then
  echo "PASS received snapshot verifies"
else
  echo "FAIL: the received snapshot does not verify"; cat "$out/verify.log" "$out/recv.log"; failed=1
fi
echo "Logs: $out"
[ -z "$failed" ]
