# deploy/ — blockfall gateway hosting

Production gateway: **`blockfall.opensensor.io:27016`** (control) with relay
data ports **27017–27216** (UDP). Runs on the opensensor Threadripper
(`192.168.53.187`) as a rootful podman **host-network** container.

## Why not the kind cluster

`opensensor-local` (kind-on-rootless-podman, user `opensensor-hosting`) hosts
the other services behind `extraPortMappings` → host **loopback** → proxy, all
TCP. The gateway needs 201 inbound **UDP** ports; kind port maps are immutable
cluster config (recreation = downtime for everything else) and every packet
would take two netavark NAT hops. A host-network sibling container is the
same podman/quadlet management model with zero NAT. If the cluster is ever
recreated, the `extraPortMappings` block for UDP 27016–27216 can move it in.

## Pieces

| File | Role |
| --- | --- |
| `quadlet/blockfall-gateway.container` | the relay: `localhost/blockfall-gateway:local`, host net, read-only, cap-dropped |
| `gateway-update.sh` + `blockfall-update.{service,timer}` | CI pull-agent: every 6 h, adopts the musl binary from GitHub **Releases** (public — no secrets), `--self-test`s it, builds a scratch image, restarts, verifies UDP liveness, records the tag in `/var/lib/blockfall-gateway/release` |
| `gateway-dns.py` + `blockfall-dns.{service,timer}` | DynamicDNS keeper: every 60 s keeps the `blockfall.opensensor.io` A record on the DO-managed zone pointed at this box's egress IP; a local `*G→*E` probe gates every write so a dead gateway never owns the record |
| `install-threadripper.sh` | idempotent installer (run from a repo copy on the host, as root) |

## Deploy pipeline (CI, public-repo-safe)

`git tag vX.Y.Z && git push --tags` → `release.yml` builds + self-tests the
musl binary and attaches `blockfall-gateway-<tag>-x86_64-unknown-linux-musl.tar.gz`
→ the pull-agent on the Threadripper adopts it within ≤ 6 h (or now:
`systemctl start blockfall-update.service`). CI never SSHes anywhere and the
repo never touches a secret.

**Secrets inventory (nothing in the repo):**

- `/etc/blockfall-dns.env` (root 0600, on the box): `DO_DNS_TOKEN` — a
  DigitalOcean PAT scoped to **dns:read + dns:write only**. Without it the
  keeper unit stays condition-skipped; DNS is managed by hand.

## Firewall / router

Forward **UDP 27016–27216** from the home router to `192.168.53.187`
(control + exactly 200 relay slots). LAN play never needs this; the forward is
what makes WAN room codes work.

## Ops

```sh
systemctl status blockfall-gateway        # quadlet-generated unit
podman logs -f blockfall-gateway          # banner, collisions, >=60 s summaries
systemctl start blockfall-update.service  # adopt latest release now
systemctl start blockfall-dns.service     # force a DNS sync
journalctl -u blockfall-dns.service -f
```

Room lifecycle is self-healing: restarting the container drops live matches
and hosts re-`*R` within 2 s. Rollback = re-point `:local` at an old
`localhost/blockfall-gateway:<tag>` and restart; the pull-agent won't fight
that until the next release tag appears.

## Failover (future standby)

The keeper's health gate (`*G→*E`) plus its 60 s write loop is the slot a
cloud standby plugs into: standby runs the same image + a probe that flips the
A record to itself after N failed primary probes, and this box re-claims the
record automatically once it answers healthily again.
