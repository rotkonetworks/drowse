#!/usr/bin/env bash
# Install drowse: build release binaries, place them, install + start the
# system daemon. Re-runnable. Needs sudo for the system bits.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
prefix="${PREFIX:-/usr/local}"

echo "==> building release binaries"
( cd "$repo" && cargo build --release )

echo "==> installing binaries to $prefix/bin (sudo)"
sudo install -Dm755 "$repo/target/release/drowsed" "$prefix/bin/drowsed"
sudo install -Dm755 "$repo/target/release/drowse" "$prefix/bin/drowse"

echo "==> installing systemd unit (sudo)"
sudo install -Dm644 "$repo/packaging/drowsed.service" /etc/systemd/system/drowsed.service
sudo systemctl daemon-reload
sudo systemctl enable --now drowsed.service

echo "==> waiting for the socket"
for _ in $(seq 1 20); do [ -S /run/drowsed/sock ] && break; sleep 0.3; done

echo "==> status"
drowse status || true
echo
echo "Done. Try:  drowse check   drowse offenders --top 10   drowse plan"
echo "Polybar: see packaging/polybar-drowse.ini"
