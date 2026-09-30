#!/usr/bin/env bash
# One-shot (re-runnable) installer for the blockfall gateway + agents on the
# opensensor Threadripper. Run AS ROOT from a copy of this repo on the host:
#
#   sudo bash deploy/install-threadripper.sh
#
# Idempotent: copies agents/units into place, builds the gateway image from
# crates/netplay-gateway/Dockerfile (musl, FROM scratch), smoke-tests it, and
# enables the quadlet + timers. Never clobbers an existing /etc/blockfall-dns.env.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=localhost/blockfall-gateway:local
DNS_ENV=/etc/blockfall-dns.env

echo "== preflight"
command -v podman >/dev/null || { echo "podman required"; exit 1; }
python3 -c 'import socket' >/dev/null

echo "== install agents"
install -d -m 0755 /usr/local/lib/blockfall /var/lib/blockfall
install -m 0755 "$REPO/deploy/gateway-dns.py" /usr/local/lib/blockfall/
install -m 0755 "$REPO/deploy/gateway-update.sh" /usr/local/lib/blockfall/

echo "== units"
install -m 0644 "$REPO/deploy/quadlet/blockfall-gateway.container" /etc/containers/systemd/
install -m 0644 "$REPO/deploy/blockfall-dns.service" "$REPO/deploy/blockfall-dns.timer" \
  "$REPO/deploy/blockfall-update.service" "$REPO/deploy/blockfall-update.timer" \
  /etc/systemd/system/

if [ ! -f "$DNS_ENV" ]; then
  install -m 0600 /dev/null "$DNS_ENV"
  cat > "$DNS_ENV" <<'ENVEOF'
# Blockfall gateway DNS keeper config (systemd EnvironmentFile).
# Create a DigitalOcean PAT scoped to dns:read + dns:write ONLY and uncomment:
# DO_DNS_TOKEN=
# Optional overrides:
# DNS_DOMAIN=opensensor.io
# DNS_RECORD=blockfall
# DNS_TTL=60
ENVEOF
  echo "created $DNS_ENV (token NOT set — dns keeper stays skipped until it is)"
fi

echo "== build image (musl, from scratch)"
# --network host: this host's rootful podman bridge net is broken (docker sets
# FORWARD DROP with no masquerade for netavark subnets); the gateway runtime
# is host-network too, so the build netns matches the runtime netns.
podman build --network host \
  -f "$REPO/crates/netplay-gateway/Dockerfile" -t "$IMAGE" "$REPO"

echo "== smoke: in-image self-test"
podman run --rm "$IMAGE" --self-test

echo "== enable units"
systemctl daemon-reload
# blockfall-gateway.service is QUADLET-GENERATED (lives under
# /run/systemd/generator): it cannot be `systemctl enable`d — the WantedBy=
# in the .container file is honored by the generator at every boot instead.
systemctl start blockfall-gateway.service
systemctl enable --now blockfall-dns.timer blockfall-update.timer

sleep 1
systemctl is-active --quiet blockfall-gateway.service && echo "gateway: active"
echo "verify: python3 -c 'import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.settimeout(3);s.sendto(b\"*GZZZZZ\",(\"127.0.0.1\",27016));print(s.recvfrom(64)[0])'"
echo "expect: b'*EZZZZZ'"
