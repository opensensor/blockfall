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
