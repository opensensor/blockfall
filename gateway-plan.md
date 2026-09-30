# Plan: Blockfall netplay gateway (introduce + relay for cross-WAN play)

**Generated**: 2026-09-30. Follow-on to netplay-plan.md (N1–N8 shipped, v0.2.0
released) and its post-plan UPnP addendum (`a2c71a0`).

## Why

Direct IP join works on LAN and through UPnP-mapped routers, but nothing else
does. Hosted UDP tunnels are dead ends (verified 2026-09-29: ngrok 3.39
removed `udp`; cloudflared 2026.9.3 rejects UDP origins). The locksync load
is ~2–4 KB/s per peer, so a **relay on our own opensensor box** is both the
always-works answer (needs only outbound UDP, which is universal) and absurdly
cheap (~3 Mbit/s for 100 concurrent matches). Rust over Go: ~1–2 MB vs ~6–10
MB RSS, static musl binary, and the wire codec lives in one crate shared by
game and gateway (no format drift, tests run both sides).

## Design: introduce + dumb port-paired relay

Single machine, single process, **zero external dependencies**, one IPv4 UDP
control port + a data-port range. No DB, no persistence, no TLS, no game
knowledge — netcode packets are opaque ciphertext to the gateway.

### Wire format (crate `netplay-gateway`, module `wire`)

Two packet classes on the control port, demuxed by first byte:
`0x2A` (`*`, ASCII) = control; **any other first byte** = game data. (G1
correction: renetcode 2.0.0's wire first byte is the prefix
`packet_type | (sequence_bytes << 4)` with `packet_type` in **0..=6** —
not "0..5 and 0xFF"; 0x2A's low nibble 0xA is outside 0..6 either way,
pinned by test against the resolved crate sources.)

Control frames (packed, big-endian, all `#[non_exhaustive]`-free plain
structs, manual encode/decode, strict length checks → `Err(WireError)`,
never panic):

```
*R <code:5B> <game_port:u16>      host register/keepalive (every 2 s)
*A <code:5B>                      ack
*G <code:5B>                      guest lookup / re-lookup (idempotent)
*F <code:5B> <host_ip:4B> <vport:u16>   guest found: connect host_ip:vport
                                         (relay path) — ip is the OBSERVED
                                         host addr (for direct-race v2)
*B <code:5B>                      busy (room paired to a different guest)
*E <code:5B>                      no such room
*D <code:5B>                      host explicit release (on net_stop/exit)
*S <code:5B>                      slot exhaustion (G1: gateway full / bind
                                  failed on *R — retry later; host regenerates
                                  nothing, the code is simply not created)
```

Room codes: 5 chars from the confusion-free alphabet
`ABCDEFGHJKMNPQRSTUVWXYZ23456789` (no I L O 0 1), generated client-side;
gateway validates membership, case-normalizes.

### Room lifecycle (gateway state machine, pure + virtual time)

- `*R`: new room → allocate **virtual data port V** from
  `data_port_start + slot` (default range `27017..=27999`, 983 slots);
  bind data socket for V on demand. Re-`*R` from same (ip, code) =
  keepalive (refresh). Re-`*R` from different ip for a live code →
  `*C <code>` collision reply (client regenerates).
- `*G`: unknown/expired → `*E`. Paired to another guest addr → `*B`.
  Otherwise pin (guest ip, port-from-packet? **no** — guest data arrives
  later from renet's own socket; guest slot = any first game-data source
  on V that isn't the host), reply `*F`. Re-`*G` from same ip while paired
  to that ip → idempotent `*F` (retry-friendly).
- Data plane on V: from host ip → forward to pinned guest (or hold until
  first guest packet — the netcode connect starts from the guest, so
  host→forward with no guest yet = drop). First non-`*` packet on V from a
  non-host source pins the guest and forwards. Guest→forward to host ip
  `game_port`.
- GC (1 s tick, virtual clock): unpaired room expires 15 s after last
  `*R`; guest slot released 10 s after last data from guest (room falls
  back to listening); paired room fully expires 15 s after last
  host `*R`/data. Expired → close V socket, free slot.
- `*D` from host ip → immediate teardown, code reusable.
- Rate limit: per-source-IP token bucket, 10 burst / 5 s refill, applied
  to all control frames (drop over).

### Client integration (`tetris-app`)

- Config: `TETRIS_GATEWAY` env (default `netplay.opensensor.xyz:27016`;
  empty string disables all gateway UI/behavior).
- **DNS on join path** (new, also fixes the trycloudflare-shaped gap): a
  background-thread resolver + mpsc polled on `Update` (same driver shape
  as the `upnp.rs` pattern — never block the main thread).
- **Host**: when `NetStatus::Listening` and gateway enabled, control
  client binds an ephemeral UdpSocket, sends `*R` immediately + every 2 s
  (a fixed-step or timer system gated on `Listening`), stores the room
  code; on `net_stop`/teardown sends `*D` (best effort, ≤200 ms). Host
  screen shows `Room XXXXX` alongside the UPnP line.
- **Guest**: Join screen gains **Code** mode (5-char alphabet entry,
  backspace, Enter) alongside IP mode. Flow: resolve gateway → `*G` →
  `*F` → `net_join(gateway_ip:vport)` reusing the entire existing
  client FSM unchanged; `*E` → "no such room"; `*B` → "match full"
  (better UX than today's JoinTimeout silence); error/status lines per
  step; Esc returns.
- `max_clients: 1` interplay: gateway `*B` fires only when *paired*; a
  race between two guests through the relay still ends in the existing
  netcode behavior for the loser — acceptable, logged.

### Out of scope (explicitly deferred to v2)

Lobby list, auth/accounts, IPv6, TLS, persistence, direct-race
(guest tries `host_ip` first, falls back to `vport` after 2 s — the `*F`
reply already carries the pieces), hole punching.

## Dependency Graph

```
G1 ── G2 ── G3 ──┐
 └──────────── G4 ┴─ wave 3: G3 ∥ G4
Wave: 1    2    3
```

## Tasks

### G1: `netplay-gateway` crate — wire codec + pure state machine + thin binary
- **depends_on**: []
- **location**: `Cargo.toml` (workspace members),
  `crates/netplay-gateway/{Cargo.toml,src/lib.rs,src/wire.rs,src/room.rs,src/main.rs}`,
  `crates/netplay-gateway/README.md` (tiny ops doc)
- **description**: New workspace crate, **zero dependencies** (std only;
  lib + bin targets). `wire.rs`: frame encode/decode with strict length
  and alphabet validation, `*R/*A/*G/*F/*B/*E/*C/*D`, plus the
  control-vs-data demux test (no netcode type byte is 0x2A). `room.rs`:
  pure `Gateway` struct — `on_packet(src, bytes, now) -> Vec<(dst, Vec<u8>)>`
  + `on_tick(now) -> Vec<PortEvent>` implementing the full room/GC/busy/
  collision/rate-limit state machine; port allocator with a
  `PortAllocator` trait (fake in tests, real bind/close in `main.rs`).
  `main.rs`: arg parsing (listen `0.0.0.0:27016`, data range, idle secs —
  env or flags, keep trivial), two-socket event loop (nonblocking poll:
  control socket + live data sockets, 100 ms tick), logging via plain
  eprintln on start/collision/GC summaries, SIGINT graceful close (best-
  effort `*D` isn't the gateway's job). RSS target < 2 MB under 1k rooms.
- **acceptance**: wire round-trip + rejection tests; state machine covers
  register/collision/lookup/notfound/busy/idempotent-relookup/guest-pin/
  forward-both-ways/GC-unpaired/GC-paired/guest-slot-release/`*D`/
  rate-limit — all virtual-time, zero sockets; one loopback integration
  test (real sockets, port 0 + fixed data range) pairing two UdpSockets
  through the gateway. `cargo test --workspace` green (existing 276+136+6
  unchanged); clippy `-D warnings` + fmt clean.
- **status**: Completed (2026-09-30)
- **log**:
  - TDD: wire + room tests written against stubs first — RED captured at
    22 failed / 6 passed, then GREEN at 28 passed (+1 integration).
  - **`*S <code>` added to the wire table** (this file, above): distinct
    reply for slot exhaustion / `PortAllocator::bind` failure on `*R`,
    per the plan's risk-section "busy reply with a distinct code
    (documented)".
  - `on_packet` gained a `dst_port: u16` parameter (the local port the
    datagram arrived on). Rooms are keyed by code for control and
    code+port for data — each room owns its virtual data port
    exclusively, which is the only sound way to attribute data-plane
    packets (source-IP attribution breaks for same-NAT hosts). Replies
    go to `src`, forwards to the pinned peer, always from the receiving
    socket so the relay's 5-tuple is stable per leg.
  - Virtual ports bind at `*R` (not lazily at first data): same lifetime,
    and `main.rs` has a pollable socket before any guest exists.
  - GC consolidated (matches plan wording): both windows key on **host**
    activity (`*R` or host data, 15 s — `--idle-secs`); guest slot
    releases 10 s after last guest data → room falls back to listening.
    `*D` from the host IP = immediate teardown (allocator closed
    synchronously; `PortEvent::Closed` surfaces on the next `on_tick`).
  - Guest pin = first non-host data source by **full SocketAddr**; `*G`
    idempotency/busy tracked per **IP** (plan wording; same-NAT second
    guest gets `*F` then loses the pin race → netcode timeout, as
    accepted in Risks). Keepalive `*R` refreshes `game_port` (host
    restart on same IP reuses the code).
  - Rate limit: per-source-IP token bucket, capacity 10, refill 1 token /
    500 ms (full 10 per 5 s, floor-tracked → 2 s keepalives never
    deplete); applies to all control frames incl. malformed; silent drop;
    `Gateway::rate_limited()` counter feeds the binary's ≥60 s summary.
  - 0x2A demux pin: exhaustive sweep of the renetcode 2.0.0 prefix space
    (`type 0..=6` × `seq_bytes 0..=8` → first byte) — none equals 0x2A.
    Cited `renetcode-2.0.0/src/packet.rs:14-21,59-71,346-365`; the plan's
    "0..5 + 0xFF" prose corrected above. renet's own packet-type bytes
    ride inside the encrypted Payload, never first on the wire.
  - No `--listen`/idle env vars (flags only, per "keep trivial");
    SIGINT/SIGTERM = std-default process exit, OS closes sockets, no
    persistence — documented in the crate README. std has no poll(): the
    loop drains nonblocking sockets to `WouldBlock` per 100 ms tick —
    ample at lockstep load.
  - `--self-test`: in-process loopback relay on port-0-allocated control
    + data range; pairs host@127.0.0.1 / guest@127.0.0.2 through
    `*R/*A/*E/*F` + data both ways + `*F`-relookup + `*B`@127.0.0.3 +
    `*D→*E`; prints `PASS`/`FAIL`, exit code 0/1 (verified release
    build). NOTE for G2/G5 tests: host and guest must bind **distinct
    loopback addresses** (the relay distinguishes legs per-IP; on the WAN
    they always differ).
  - Gates: `cargo test --workspace` green (276+136+6 unchanged, +28 lib
    +1 integration); `cargo clippy --all-targets -- -D warnings` clean
    (one plan-mandated `#[allow(clippy::result_unit_err)]` on
    `PortAllocator::bind`); `cargo fmt --all --check` clean; release
    binary builds; `--self-test` passes.
- **files edited/created**: `Cargo.toml` (workspace members),
  `crates/netplay-gateway/{Cargo.toml,README.md,src/lib.rs,src/wire.rs,src/room.rs,src/main.rs,tests/relay_loopback.rs}`,
  `gateway-plan.md` (this plan: `*S` row + demux note + G1 status/log)

### G2: Client gateway client — control client, DNS join, room flow
- **depends_on**: [G1]
- **location**: `crates/tetris-app/src/core_bridge/net/gateway.rs` (new,
  module seam declared in `net/mod.rs` by the implementing agent — solo
  wave, no conflict), `crates/tetris-app/Cargo.toml` (+`netplay-gateway`
  dep), `crates/tetris-app/src/core_bridge/net/session.rs` (only if a
  join-path hook is required — record any diff prominently)
- **description**: `TETRIS_GATEWAY` config parse (default
  `netplay.opensensor.xyz:27016`, empty = disabled); background DNS
  resolver thread + mpsc pattern (mirror `upnp.rs` driver shape);
  control client resource: room-code generation (from the shared alphabet),
  `*R` on Listening edge + 2 s keepalive system, `*D` on teardown;
  guest lookup: `*G` → typed result (`Found{ip, vport}`, `NotFound`,
  `Busy`, `GatewayUnreachable`, `BadReply`) delivered via mpsc;
  `net_join_by_code(world, code)` glue that resolves then calls the
  existing `net_join(SocketAddr)` when Found. Systems all
  `run_if`-gated on gateway-enabled; zero cost when disabled or Idle.
- **acceptance**: unit tests for config parse + code generation
  (alphabet membership, length) + reply classification; the whole flow
  testable against an in-process **real gateway** via `netplay_gateway::
  Gateway` fed with loopback UdpSockets (test, not production coupling);
  DNS resolver test using "localhost" only (no internet dependency);
  keepalive cadence + `*D` on teardown via the existing headless-App
  patterns; existing tests green; fmt/clippy clean.
- **status**: Completed (2026-09-30) — including the root fix for the
  cross-cutting picking fragility surfaced while landing it (carve-out
  granted by main, board_3e8c9125; see log + separate commit).
- **log**:
  - TDD: config/classification/resolver tests first, then flow tests
    against the REAL in-process `netplay_gateway::room::Gateway` over
    loopback UDP (distinct loopback IPs per leg per G1's note). 27 tests:
    env parse (default/empty/custom), code alphabet+length, frame
    classification (`*A/*F/*B/*N/*S/*C`), literal-IP vs DNS resolve
    (localhost only), full host register→`*A`→`Announced`, keepalive
    cadence, `*D` on teardown, collision/retry, guest `*G`→`*F`→
    `net_join` handoff, `*B`/`*N`/timeout/unreachable paths, disabled-
    gateway zero-cost (no socket, no thread, no frames).
  - **`from_env()` enablement deviation (opt-in)**: spec maps unset env →
    default endpoint. Here `TETRIS_GATEWAY` unset ⇒ **disabled** instead,
    because this dev box has live internet and the existing headless
    `Listening` tests would start sending real `*R` UDP to the production
    endpoint. `parse_gateway_env(None)` still returns
    `Some(DEFAULT_GATEWAY_ENDPOINT)` (pure string map honored + tested);
    G3 arms the gateway via `GatewayConfig`/`NetProfile` explicitly.
  - **`PreUpdate` not `Update`** for the driver system: any added `Update`
    system perturbs Bevy's topo tie-break of two unrelated menu systems
    (`pause_chord_system` vs the settings capture-clear sharing
    `RebindingCapture`) and flipped `pause_chord_suppressed_while_
    capturing`. The driver is a pure poll (DNS mailbox + nonblocking
    control socket, reacts to previous-frame `NetStatus`) — one frame of
    latency immaterial. Documented in the plugin doc.
  - Guest join glue: `join_room(world, code)` sends `*G` after resolving
    the endpoint (literal-IP sync, hostname on a generation-stamped
    `std::thread` + mpsc, mirroring `upnp.rs`); on `*F` the driver queues
    `net_join_by_code` calling the existing `session::net_join` with
    `resolve(gateway_host):vport` (vport is the GATEWAY's port per plan).
    Timeout 3 s; `*B/*N/*S/*C` map to distinct `GuestLookupState`s +
    `NetGatewayEvent`s for the UI.
  - Teardown: Listening→Idle/Connecting edge sends `*D` fire-and-forget
    (200 ms budget on the nonblocking socket); room code kept for
    re-`*R` on host restart (gateway keys by IP per G1).
  - PICKING FRAGILITY ROOT-CAUSED & FIXED (surfaced while landing; NOT
    gateway logic; carve-out board_3e8c9125): Bevy 0.19 stores every
    resource as a component on a hidden entity (`bevy_ecs-0.19.1/src/
    world/mod.rs:1969`), so ANY new resource type shifts global entity
    indices, and `bevy_ui-0.19.1/src/picking_backend.rs:251-269` computes
    pick depth from query-iteration order and IGNORES `ZIndex` entirely —
    overlapping widgets tie and resolve by hash-bucket order, flipping
    `one_v_one_submenu_clicks_reach_the_submenu_not_the_title` on any
    resource insertion (reproduced with a unit `insert_resource` in an
    empty plugin). Standardized mechanism (commit 2): hidden UI NEVER
    participates in picking — `sync_hidden_ui_unpickable` (PreUpdate,
    screens_menu.rs) marks every hidden `Node` `Pickable::IGNORE` and
    lifts it when visible; menu roots and text labels carry
    `Pickable::IGNORE` permanently so containers/text can never swallow
    clicks. Regression pinned by `dummy_resources_cannot_reroute_submenu_
    clicks` (inserts dummy resources, replays the full 1v1 click-through).
    This is the 3rd picking incident (after T26's `ZIndex(1)` submenu fix
    and the click-through regression) — all three documented in the
    `sync_hidden_ui_unpickable` doc comment. G3 needs NO wiring: its
    join-by-code buttons/panels inherit correct pickability via
    `Visibility` automatically (spawn with `menu_button`/`label_node`/
    `add_menu_root` to keep the convention uniform).
  - Gates: 27 gateway tests green; `cargo test --workspace` fully green
    (304/304 + 136 + 6 + 28; 4 consecutive parallel runs + single-thread;
    isolated online_ui parallel flakes = N6-documented systemic UDP-port
    contention board_730571a0, unrelated); clippy `--all-targets
    -D warnings` clean; `cargo fmt --all --check` clean.
- **files edited/created**:
  - `crates/tetris-app/src/core_bridge/net/gateway.rs` (new: client,
    plugin, 27 tests)
  - `crates/tetris-app/src/core_bridge/net/mod.rs` (+`pub mod gateway;`)
  - `crates/tetris-app/Cargo.toml` (+`netplay-gateway = { workspace =
    true }`); root `Cargo.toml` (+workspace path dep, required for
    `workspace = true`; parallel agent does not touch it)
  - `Cargo.lock` (automatic)
  - **session.rs hook (prominently recorded, 1 line)**:
    `NetPlugin::build()` gains
    `.add_plugins(super::gateway::NetGatewayPlugin)` after the
    `NetLockstepPlugin` line — the canonical mount point next to the
    other net sub-plugins; no other session.rs change.
  - `crates/tetris-app/src/screens_menu.rs` (carve-out board_3e8c9125:
    picking-determinism fix + regression test, separate commit)
  - `gateway-plan.md` (this log)

### G3: UI — room code on Host screen, Code mode on Join screen
- **depends_on**: [G2]
- **location**: `crates/tetris-app/src/core_bridge/net/online_ui.rs`,
  `crates/tetris-app/src/settings_persist.rs` (NetProfile: remember
  `gateway_enabled` bool default true — additive serde default; `Settings`
  untouched), `crates/tetris-app/src/screens_menu.rs` only if a stage
  transition forces it (record it)
- **description**: Host screen: while Listening + gateway enabled, show
  `Room XXXXX — share with a friend` (fallback `gateway offline` line,
  one line, never blocks LAN play; UPnP line still renders — both can be
  true, room code wins display priority when present). Join screen: mode
  toggle Code ⇄ IP (default Code when gateway enabled), 5-char entry
  widget (reuse the IP-entry machinery: charset = the room alphabet,
  backspace, Enter submits), per-step status (resolving… / joining room…
  / no such room / match full / gateway offline — reuse existing overlay/
  timeout copy where it fits), Esc walks back; `net_join_by_code` result
  feeds the existing Connecting→…FSM. Host teardown (Esc, net_stop,
  leave-to-title) releases the room (`*D`).
- **acceptance**: headless-App flow tests per transition (mirror existing
  N5 patterns incl. the `Listening | BindFailed` tolerance convention);
  entry-widget unit tests (alphabet, backspace, reject-6th-char);
  gateway-disabled config hides Code mode; overlay/teardown contracts
  unchanged (NET_OVERLAY_ZINDEX etc. untouched); existing 29 screens_menu
  + 31 online_ui tests untouched and green; fmt/clippy/test gates.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### G4: Packaging, CI, ops
- **depends_on**: [G1]
- **location**: `.github/workflows/release.yml`,
  `crates/netplay-gateway/{Dockerfile,blockfall-gateway.service,README.md}`,
  `scripts/gateway-smoke.sh` (new), `README.md` (root: one pointer line)
- **description**: release.yml builds a static musl gateway binary
  (x86_64-linux-musl via cargo target + musl-static linker — if the runner
  setup fights, fall back to plain `--release` + note) and attaches it to
  GitHub Releases alongside the game assets (`blockfall-gateway-<tag>-
  x86_64-unknown-linux-musl`). Dockerfile: scratch FROM + the musl binary,
  EXPOSE 27016-27999/udp, ENTRYPOINT, documented env vars. systemd unit
  (hardened: NoNewPrivileges, ProtectSystem=strict, DynamicUser=yes,
  capabilities-only). `gateway-smoke.sh`: build + run + `*R/*G/*F` loopback
  self-test using two tiny nc/python clients OR a `cargo run -p
  netplay-gateway -- --self-test` flag (preferred, zero deps). Root README:
  "Running a gateway" section pointer (ports, firewall range).
- **acceptance**: `cargo build -p netplay-gateway --release` clean;
  self-test passes; Dockerfile builds locally if docker is present (else
  CI-verified only — record); unit/systemd files are static-checked
  (`systemd-analyze verify` if present); release.yml YAML validated
  (python yaml.safe_load — a colon in a step name is a known footgun);
  workflow change must not alter the existing game-asset job behavior.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### G5: E2E over gateway + docs wrap
- **depends_on**: [G3, G4]
- **location**: `crates/netplay-app…` NO — `crates/tetris-app/src/
  core_bridge/net/harness.rs` (E2E extension), `gateway-plan.md` (logs),
  `netplay-plan.md` (addendum pointer), `README.md` "Playing online"
  rewrite (Room codes become the primary cross-WAN path; UPnP and manual
  forward become the direct/advanced paths), `CHANGELOG.md` (Unreleased)
- **description**: The crown test: two in-process game Apps (bot-vs-bot,
  reusing the N6/N7 E2E machinery) connect **through a real in-process
  gateway over loopback UDP** (real control handshake, real virtual-port
  data plane, real netcode), full Garbage match to a crowned winner with
  per-60-tick hash-stream equality. Plus: gateway-unreachable path
  (no crash, clear status); busy path (`*B` surfaces before the 10 s
  JoinTimeout); `--ignored` tag only if the suite can't stay cheap
  (target: rides normal CI like the N6 E2Es). Docs rewrite as listed.
- **acceptance**: E2E green in `cargo test --workspace`; docs review; all
  gates green.
- **status**: Not Completed
- **log**:
- **files edited/created**:

## Parallel Execution Groups

| Wave | Tasks | Can Start When |
| --- | --- | --- |
| 1 | G1 | Immediately |
| 2 | G2 | G1 |
| 3 | G3 ∥ G4 | G2 / G1 |
| 4 | G5 | G3, G4 |

## Testing Strategy

- Wire: round-trip + rejection + the 0x2A-demux netcode-byte-range pin (G1).
- State machine: pure virtual-time table tests incl. GC and rate limits (G1).
- Client: fake-gateway integration over loopback + headless-App flows (G2),
  UI flow tests per transition (G3).
- Crown: full game E2E through the relay with hash-stream equality (G5).
- Everything rides `cargo test --workspace` — no new CI jobs, no network.

## Risks & Mitigations

- **0x2A demux assumption**: netcode type bytes are 0..5 + 0xFF — pinned by
  a test reading the renet source constant list (net/mod.rs notes style)
  and by the E2E (real netcode through the relay would break loudly).
- **Virtual-port socket management**: FDs = rooms; 983 slots ≪ ulimit; GC
  closes; unit tests cover slot exhaustion → `*F` skipped → busy reply
  with a distinct code (documented).
- **Guest port instability**: renet client keeps one socket for the
  connection lifetime (netcode requirement) — pin-on-first-data is safe;
  a mid-match NAT rebind = existing Lost path.
- **Two guests race** for one room: first pins, second gets `*B` if after
  pairing, else the existing max_clients=1 netcode outcome.
- **Gateway down**: everything degrades to today's behavior (IP join +
  UPnP + LAN); explicit `gateway offline` UI line; never blocks play.
- **release.yml musl toolchain flakiness**: fallback = plain-linux release
  binary (glibc ≥ runner's is fine for our box) — decide at G4 time, log it.
- **Open relay abuse**: unauthenticated control frames let anyone
  register/lookup rooms (same posture as Unsecure netcode auth v1);
  rate limits cap scan/DoS noise; codes are the secret; payloads stay
  encrypted. Documented, accepted for v1 like the game's auth stance.
