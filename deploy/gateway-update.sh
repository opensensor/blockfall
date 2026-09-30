#!/usr/bin/env bash
# Blockfall gateway CI pull-agent. The gateway lives behind home NAT where CI
# cannot reach it, so deployment is a PULL: GitHub CI publishes the static
# musl binary on every tag (release.yml "gateway" job) and this agent adopts
# it. Nothing secret on the host: the repo and its releases are public.
#
# Flow: check releases/latest -> if newer than state marker -> download the
# musl tarball -> run `--self-test` (the release gate, now also the install
# gate) -> build a FROM-scratch image around the binary -> restart the quadlet
# -> verify live + UDP liveness -> record the tag. Any failure leaves the
# previous container running (the marker is only written on full success).
set -euo pipefail

REPO=opensensor/blockfall
ASSET_RE='^blockfall-gateway-.*-x86_64-unknown-linux-musl\.tar\.gz$'
STATE=/var/lib/blockfall-gateway
IMAGE=localhost/blockfall-gateway
SERVICE=blockfall-gateway.service
CTRL_PORT=${CTRL_PORT:-27016}

lock=/run/lock/blockfall-update
exec 9>"$lock"
flock -n 9 || { echo "update already running"; exit 0; }

log() { echo "[blockfall-update] $*"; }

json() { python3 -c "import json,sys;print(json.load(sys.stdin)$1)"; }

latest=$(curl -fsSL --max-time 20 -H 'Accept: application/vnd.github+json' \
  "https://api.github.com/repos/$REPO/releases/latest")
tag=$(printf '%s' "$latest" | json "['tag_name']")
asset=$(printf '%s' "$latest" | python3 -c '
import json, sys, re
d = json.load(sys.stdin)
pat = re.compile(sys.argv[1])
for a in d.get("assets", []):
    if pat.match(a["name"]):
        print(a["browser_download_url"]); break
' "$ASSET_RE")

current=""
[ -f "$STATE/release" ] && current=$(cat "$STATE/release")
if [ -z "$asset" ]; then
  log "no musl gateway asset on $tag (pre-gateway release?) — skipping"
  exit 0
fi
if [ "$current" = "$tag" ]; then
  exit 0
fi
log "adopting $tag (current: ${current:-none})"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL --max-time 120 -o "$work/asset.tar.gz" "$asset"
tar xzf "$work/asset.tar.gz" -C "$work"
mv "$work/netplay-gateway" "$work/gateway"
chmod +x "$work/gateway"

"$work/gateway" --self-test || { echo "self-test FAILED — keeping ${current:-previous} version"; exit 1; }

printf 'FROM scratch\nCOPY gateway /blockfall-gateway\n' > "$work/Containerfile"
podman build -q -t "$IMAGE:$tag" -t "$IMAGE:local" -f "$work/Containerfile" "$work" >/dev/null

systemctl restart "$SERVICE"
sleep 1
systemctl is-active --quiet "$SERVICE" || { echo "service failed to start — image :local points at $tag, inspect: podman logs blockfall-gateway"; exit 1; }

# Live UDP liveness: *G on an unlikely code must be answered *E.
python3 - "$CTRL_PORT" <<'EOF' || { echo "gateway not answering on $CTRL_PORT"; exit 1; }
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(3)
s.sendto(b"*GZZZZZ", ("127.0.0.1", int(sys.argv[1])))
sys.exit(0 if s.recvfrom(64)[0] == b"*EZZZZZ" else 1)
EOF

mkdir -p "$STATE"
echo "$tag" > "$STATE/release"
log "now running $tag"
