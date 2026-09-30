# netplay-gateway — ops

Introduce + port-paired UDP relay for Blockfall cross-WAN 1v1
(gateway-plan.md). Single static binary, **zero dependencies**, one control
port + a data-port range. Nothing to install, nothing persists.

## Build

```sh
cargo build -p netplay-gateway --release   # target/release/netplay-gateway
```

## Run

```sh
netplay-gateway                                  # defaults: 0.0.0.0:27016, data 27017..=27999, idle 15 s
netplay-gateway --listen 0.0.0.0:27016 --data-start 27017 --data-count 983 --idle-secs 15
```

Flags are the only config (no env vars, by design). `--self-test` runs an
in-process loopback relay test and exits `0`/`1` with `PASS`/`FAIL` — the
CI/ops smoke entry:

```sh
netplay-gateway --self-test
```

## Firewall

Allow inbound **UDP 27016–27999** (one control port + 983 relay data ports;
adjust with `--data-start/--data-count`, keep the whole range open).

## Resource profile

RSS target < 2 MB for 1k rooms: one UDP socket per live room (983 max ≪
ulimit), slots freed by GC (`--idle-secs` host window, 10 s guest-slot
window). Locksync load is ~2–4 KB/s per peer — ~3 Mbit/s for 100 concurrent
matches. The poll loop wakes at 100 ms; idle CPU is negligible.

## Security posture (v1, accepted)

- Control frames are **unauthenticated** — same stance as the game's
  Unsecure netcode auth v1. Room codes are the shared secret; payloads stay
  end-to-end encrypted by netcode (the relay only sees opaque bytes).
- Per-source-IP token bucket (10 burst / 5 s refill) rate-limits all
  control frames; over budget is silently dropped (counted in the periodic
  log summary).
- Strict wire decoding (never panics on hostile bytes), IPv4-only, no game
  knowledge, no persistence.

## Lifecycle

SIGINT/SIGTERM terminate the process (std defaults); the OS closes all
sockets — restart is always safe. Logs: startup banner, collision lines,
GC port closes, and a ≥60 s summary (`rooms`, `ctrl_rx`, `fwd_bytes`,
`rate_limited`).

## Deploying

### Docker

Multi-stage Dockerfile (musl builder → `FROM scratch`, one static binary, no
shell). Build from the **repo root**, then run with host networking —
mapping 984 UDP ports with `-p` is slow; the firewall note below applies to
the host either way:

```sh
docker build -f crates/netplay-gateway/Dockerfile -t blockfall-gateway .
docker run -d --name blockfall-gateway --net=host --restart=unless-stopped blockfall-gateway
docker run --rm blockfall-gateway --self-test    # in-container smoke check
```

Flag overrides go after the image name (CLI flags are the only config).

### systemd

`blockfall-gateway.service` is the hardened unit (DynamicUser,
`ProtectSystem=strict`, no privileges needed — all ports are > 1024; the
unit's capability comment explains the one droppable line). Install the
static **musl** release binary from the GitHub release (or
`cargo build --release --target x86_64-unknown-linux-musl`) at the unit's
`ExecStart=` path:

```sh
tar xzf blockfall-gateway-<tag>-x86_64-unknown-linux-musl.tar.gz
sudo install -m 0755 netplay-gateway /usr/local/bin/blockfall-gateway
sudo cp crates/netplay-gateway/blockfall-gateway.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now blockfall-gateway
journalctl -u blockfall-gateway -f
```

### Firewall & upgrades

Open inbound **UDP 27016–27999** (whole range — see Firewall above).
Upgrading is **replace the binary, restart**: nothing persists, live rooms
drop and re-register within seconds (hosts retry `*R` every 2 s). Watch the
≥60 s summary line in the log (`rooms`, `ctrl_rx`, `fwd_bytes`,
`rate_limited`) after deploying — nonzero `rate_limited` growth with no
room churn means a scanner; that is what the bucket is for.

## Self-test

```sh
target/release/netplay-gateway --self-test   # PASS → exit 0, FAIL → exit 1
```

Pairs a host and two guests through an in-process relay on ephemeral
loopback ports — full lifecycle (`*R/*A` register, `*G/*F` lookup, guest
pin, forward both ways, `*B` busy, idempotent re-lookup, `*D` teardown).
No external services, no fixed ports; it is the CI gate in
`.github/workflows/release.yml` and what `scripts/gateway-smoke.sh` wraps.
Legs are distinguished per-IP, so the test binds 127.0.0.1/.2/.3 — all
local on Linux's `lo`; CI/smoke targets Linux hosts.
