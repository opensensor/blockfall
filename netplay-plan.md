# Plan: Networked 1v1 Opponent Support (Netplay)

**Generated**: 2026-09-28 (rev 2 — post-review: net module re-homed under
`core_bridge/`, CI strategy reworked, input/pause/rematch/teardown contracts
made explicit, N7 re-scoped after verifying the core is integer-only)

## Overview

Adds online 1v1 versus to Blockfall on top of the existing local versus stack
(T24–T26). Design decisions, confirmed with the author:

- **Connection**: direct IP/port. Host opens a listening port; guest types
  `ip:port`. No lobby, no signaling server, no LAN discovery (post-v1).
- **Sync model**: deterministic **lockstep with input delay**. Both peers run
  the identical `tetris_core::versus::Match` from one shared seed; inputs land
  D ticks late (delay-based, default 8 fixed steps ≈ 133 ms). No rollback, no
  state streaming — the opponent's board, HUD, next/hold, and pending garbage
  all come *for free* from the local deterministic mirror. Host is the
  authoritative tick clock; periodic snapshot-hash comparison detects desync.
- **Transport**: `bevy_renet` 5.0.0 (crates.io verified 2026-09-28: depends
  on `bevy_app ^0.19`, compatible with the pinned Bevy 0.19.1) with its
  default **netcode** transport (encrypted UDP, connection management,
  ReliableOrdered / ReliableUnordered / Unreliable channels). MIT/Apache —
  compatible with the GPL-3.0 app.

Integration philosophy mirrors T25: the net layer lives **under the existing
bridge** as `crates/tetris-app/src/core_bridge/net/` (declared by
`core_bridge/mod.rs`, which already declares `mod versus;`), and `NetPlugin`
is mounted from `CoreBridgePlugin::build()` — the exact precedent
`VersusBridgePlugin` uses. **`main.rs` stays frozen** (it is the only crate
root: `mod` declarations at `main.rs:9-18`, the only production plugin list at
`main.rs:50-60`). The net systems **gate the existing versus systems in and
out**; the `tetris-core` public API stays frozen; solo play is untouched; the
`AppState` enum is unchanged (net UI gates on `NetSession` status, like T26's
`VersusFlow` stage machine).

Determinism premise, verified at plan time: `tetris-core` contains **zero**
`f32`/`f64` (timers are `u32`/`u64` tick counters, e.g.
`game.rs:77-78`), and `Action`, `MatchSnapshot`, `GameSnapshot`, `Board` all
derive `Serialize + Deserialize` (`actions.rs:11`, `versus.rs:158`,
`game.rs:37`, `board.rs:22`). Lockstep mirrors are therefore cross-platform
bit-exact by construction; no FP/FMA caveat is needed.

Existing hooks this plan stands on:
- `tetris_core::versus::Match` — `new(seed, rule)`, `apply(side, action)`,
  `tick(side)`, `snapshot()`; serde types; fully deterministic from seed +
  actions (proven by T24 tests + nightly soak).
- `core_bridge/versus.rs` — `VersusMatch` (NonSend), `Controller {Human,Bot}`,
  `VersusWinner`, `VersusEvent`, `start_versus`/`end_versus`, the 60 Hz
  `versus_bridge_system` drain-then-tick order (actions first, then
  `Match::tick` left,right — `core_bridge/versus.rs:256-264`),
  `versus_bot_system`, `versus_restart_on_r_system` (R on `Update`), the
  `TETRIS_1V1` in-process env-harness pattern.
- `input.rs` — `VersusActions {left, right}`, `VersusBindings` P1/P2 presets,
  the lone-human preset override (`input.rs:742-764` — `lone_human_p1`
  currently triggers whenever `p2` is not `Human`: **N4 must account for it**,
  see below), DAS/ARR via `ShiftRepeat`/`RepeatTimer`.
- `screens_menu.rs` — `VersusFlow` stage machine, winner overlay
  (`winner_text` at :275 exhaustively matches `Controller`), pause chord
  (`pause_chord_system` :507-532), `VersusRematchButton` →
  `rematch_versus` → `start_versus` (which re-seeds **locally** — dangerous
  for mirrors, gated in N5), plus two recorded visibility regressions
  (:1646-1693 click-swallowing active versus HUD; :960-975/:2055-2062
  ZIndex click-through) that N3/N5's teardown contract must respect.
- `core_bridge/mod.rs:573` — `CoreBridgePlugin::build()` mounts
  `VersusBridgePlugin`; same slot mounts `NetPlugin`.
- `settings_persist.rs` — load/save helpers reused by a **separate**
  `net_profile` persistence (the T1 contract in `state.rs:1-3` forbids
  reshaping `Settings`).

## Wire protocol (agreed design, implemented in N1/N2/N3)

Channel usage (client→server = guest→host; server→client = host→guest).
Send types are **design intent** — N1's spike finalizes exact renet 2.0
channel/message APIs (note: `resend_time` is a `ReliableUnordered` knob;
ReliableOrdered resends continuously — do not treat the table as verified
API):

| Msg | Dir | Channel (intent) | Notes |
| --- | --- | --- | --- |
| `Hello { version, delay }` | guest→host | ReliableOrdered | version must equal `PROTOCOL_VERSION`; `delay` = guest's desired input delay, both sides adopt `D = max(host, guest)` |
| `MatchStart { seed, rule }` | host→guest | ReliableOrdered | guest never starts play without it; also implements **rematch** (host sends a fresh one) |
| `TickInput { tick, actions }` | guest→host | ReliableOrdered | arrives ≈D ticks early; D absorbs jitter |
| `TickBatch { tick, left, right }` | host→guest | ReliableOrdered | sent **every** tick (empty lists included) — doubles as clock pacing; batch T is emitted when tick T−D executes |
| `SnapshotHash { side, tick, left, right }` | both | ReliableUnordered | every 60 ticks; mismatch → desync teardown |
| `Bye` | either | ReliableUnordered | clean exit → peer tears down to title overlay |

- Roles: host = netcode **server** (`max_clients: 1`) and always
  `Side::Left`; guest = netcode **client** and always `Side::Right` —
  identical to the local P1/P2 split, so T26's versus HUD/viewport code is
  reused unchanged.
- Tick clock: the host's counter over `FixedUpdate` steps is authoritative;
  the guest executes batches strictly in tick order. A tick whose remote
  input hasn't arrived executes with an empty action list (never wait — the
  hash check would otherwise flag a stall as divergence; late inputs are
  dropped and logged, which is exactly why `D` is negotiated as `max`).
- Input path: each side's local player queues actions for tick `T + D`; they
  are queued locally AND sent immediately as `TickInput { T + D }`. At batch
  build time the host merges its own delayed queue with the guest's
  already-arrived `TickInput` for that tick, applies both sides to its local
  `Match`, and ships the batch; the guest mirrors it.
- Serialization: bincode 1.3 over the serde types. Snapshot hash: FNV-1a over
  the bincode bytes of `MatchSnapshot` (process-stable, unlike std `Hash`).
- Auth: v1 uses `ServerAuthentication::Unsecure` + fixed `PROTOCOL_ID`
  (netcode still rejects wrong-protocol traffic). No session tokens in v1.
- **Netcode limitation, designed around**: with `max_clients: 1` a second
  inbound client is **silently dropped** by the transport (no server event).
  "Match full" is therefore indistinguishable from "host offline" and is
  presented as a join-timeout message, not a distinct event.

## Dependency Graph

```
N1 ── N2 ── N3 ── N4 ──┬── N5 ──┐
                        │        ├── N8 (docs)
                        └── N6 ──┼── N7 (soak/audit)
                                 └────── N8
Wave:  1    2    3    4    5(N5,N6)   6(N7,N8)
```

## Tasks

### N1: Wire protocol codec + dependency landing + API spike
- **depends_on**: []
- **location**: `Cargo.toml` (workspace deps), `crates/tetris-app/Cargo.toml`,
  `crates/tetris-app/src/core_bridge/net/mod.rs` (new),
  `crates/tetris-app/src/core_bridge/net/protocol.rs` (new),
  `crates/tetris-app/src/core_bridge/mod.rs` (one `mod net;` line)
- **description**: Add `bevy_renet = "5.0.0"` (default netcode feature) and
  `bincode = "1.3"` to `[workspace.dependencies]` + `tetris-app` deps.
  **First action — the spike**: against the downloaded crate source, pin and
  record in a `net/mod.rs` doc section ("Verified API notes") every bevy_renet
  5.0.0 / renet 2.0 / renet_netcode 2.0 surface N2/N3 will code against:
  message payload encoding (raw bytes vs `Serialize` integration),
  `ChannelConfig`/`SendType` shapes, `RenetServerPlugin`/`RenetClientPlugin`
  resource and event types (`RenetServer`, `RenetClient`,
  `RenetServerEvent`, `RenetReceive`/`RenetSend` sets), netcode
  `ServerConfig`/`ClientAuthentication` fields, whether the bound port is
  queryable when binding port 0 (else fixed-port test strategy), transport
  drop/teardown semantics for un-listening, and connect timeout
  configurability. Then define `PROTOCOL_ID: u64`,
  `PROTOCOL_VERSION: &str`, the `NetMsg` enum (Hello{version,delay},
  MatchStart, TickInput, TickBatch, SnapshotHash, Bye — embedding
  `AttackRule`, `Action`, tick `u64`, `IoError`-free payloads),
  `encode`/`decode` helpers, `decode` tolerant of unknown variants →
  `Err(ProtocolError)`, and `snapshot_hash(&MatchSnapshot) -> u64`
  (FNV-1a over bincode bytes).
- **validation**: `cargo check --workspace`; round-trip unit tests for every
  `NetMsg` variant incl. malformed/short-buffer `decode` rejection; hash
  stability tests (same state equal, differing state differs); all existing
  tests green (do not pin an exact count); clippy `-D warnings` + fmt clean.
- **status**: Completed
- **log**: Deps landed: bevy_renet 5.0.0 / renet 2.0.0 / renet_netcode 2.0.0 (+
  new transitive `renetcode` 2.0.0 protocol crate) / bincode 1.3.3 — plan
  version assumptions held. Full verified-API notes written in
  `core_bridge/net/mod.rs` with file:line citations. Key corrections to plan
  assumptions: (1) **second-guest is NOT silently dropped** — netcode sends a
  `ConnectionDenied` packet at max_clients (`renetcode server.rs:303`), and
  the guest can read it via transport `disconnect_reason()`, so "match full"
  is distinguishable client-side (host side still gets no event); (2)
  `resend_time` exists on **both** reliable SendType variants, not just
  ReliableUnordered; (3) `ConnectionConfig` has **no timeout field** —
  timeout rides on the netcode connect token and is hard-coded
  (`expire 300 s / timeout 15 s`) for `ClientAuthentication::Unsecure`, so
  the ~10 s JoinTimeout watchdog must be app-side (correctly ordered); (4)
  `ServerConfig` also requires `current_time: Duration` and has no Default;
  (5) server transport has **no** `local_addr()` — bind the `UdpSocket`
  yourself, read `local_addr()` **before** moving it in, set
  `public_addresses` from it (client transport *does* expose `addr()`);
  (6) `RenetServerEvent`/`NetcodeErrorEvent` are Bevy 0.19 **observer
  triggers** (`On<RenetServerEvent>` via `app.add_observer`), not readable
  message queues — bridge them into `Messages<NetEvent>` from an observer;
  (7) `NetcodeServerPlugin`/`NetcodeClientPlugin` are **required in addition
  to** the two renet plugins (they own `send_packets`); (8) teardown verified:
  zero `Drop` impls anywhere, transports own their sockets → removing
  transport resources frees the port synchronously; also
  `NetcodeClientTransport::update` errors **every frame** after a
  disconnect — tear down promptly; (9) `MinimalPlugins` includes
  `TimePlugin`, so the N2 headless-test fallback concern is moot (use
  manual `app.update()`). Codec: `NetMsg` (6 variants incl.
  `MatchStart.match_delay`), `PROTOCOL_ID`, `PROTOCOL_VERSION`,
  `encode`/`decode` (bincode fixint encoding == `bincode::serialize`
  defaults, **plus** `reject_trailing_bytes` for strictness),
  `snapshot_hash` = FNV-1a-64 over bincode bytes. TDD: 7 unit tests written
  first — RED (5 failed: todo! stubs + one genuine fixture finding:
  `GameSnapshot` carries no timers, so N ticks vs N+1 at level 1 gravity is
  the *same* snapshot — hash-equality there is correct; fixture switched to
  action-driven divergence) → GREEN. All gates green: check/fmt/clippy
  -D/`cargo test --workspace` (146 app + 136 core + 6 integration).
- **files edited/created**: `Cargo.toml` (workspace deps), `Cargo.lock`,
  `crates/tetris-app/Cargo.toml`, `crates/tetris-app/src/core_bridge/mod.rs`
  (`mod net;`), `crates/tetris-app/src/core_bridge/net/mod.rs` (new — spike
  notes), `crates/tetris-app/src/core_bridge/net/protocol.rs` (new — codec +
  tests), `netplay-plan.md` (this entry)

### N2: Net session resource + plugin + connection lifecycle
- **depends_on**: [N1]
- **location**: `core_bridge/net/mod.rs`, `core_bridge/net/session.rs` (new),
  `core_bridge/mod.rs` (mount `NetPlugin` inside `CoreBridgePlugin::build()`)
- **description**: `NetPlugin` adds `RenetServerPlugin` + `RenetClientPlugin`
  and a `NetSession` resource: `role: NetRole {Host, Guest}`,
  `status: NetStatus {Idle, Listening, BindFailed(String), Connecting,
  Handshaking, Ready, InMatch, Lost(NetLossReason)}`, plus owned transport
  resources while active. Free functions: `net_host(port)` (bind
  `0.0.0.0:port`, `ServerConfig {max_clients: 1, protocol_id: PROTOCOL_ID,
  authentication: Unsecure, public_addresses: bound}`; `std::net::UdpSocket`
  bind error → `BindFailed(msg)`, no panic), `net_join(addr)`,
  `net_stop()` (drop transport resources → port released; used by Esc-on-
  Listening and all teardowns). Bridging systems on the renet sets:
  server `ClientConnected` → expect `Hello`; valid → `Ready`, wrong version →
  kick, guest mirrors: connected → `Hello{version, delay}` → on `MatchStart`
  → `InMatch`. `client_just_disconnected` / `ClientDisconnected` / connect
  timeout → `Lost(reason)`. One `NetEvent` Bevy `Message`
  (`PeerConnected`, `PeerLost(reason)`, `VersionMismatch`, `BindFailed(msg)`,
  `JoinTimeout` [≈10 s `Connecting` watchdog], `Desync {tick}`,
  `ByeReceived`) for N5's UI. Host-side delay adoption: on `Hello`,
  `D = max(local, peer)` on both sides (guest learns it from
  `MatchStart.match_delay`; add that field). renet `update`/`send_packets`
  are driven by the bevy_renet plugin — never poll renet manually.
- **validation**: `NetStatus` transition-table tests with pure logic factored
  out; an **in-process two-`App` integration test inside the crate**
  (`#[cfg(test)]` in `session.rs`): two `App`s with `MinimalPlugins` +
  `RenetServerPlugin`/`RenetClientPlugin` + netcode transports on a loopback
  port (fixed test port from `TETRIS_TEST_NET_PORT` env with a deterministic
  default, documented collision caveat), assert connect → both `Ready`, drop
  client → host `Lost`. If MinimalPlugins proves insufficient for the renet
  plugin's system-set ordering, fall back to adding `TimePlugin` and note it
  in the mod doc. clippy/fmt clean; all existing tests green.
- **status**: Completed
- **log**: `NetPlugin` mounted next to `VersusBridgePlugin` in
  `CoreBridgePlugin::build()`; it registers all four bevy_renet plugins
  (N1 correction #7 applied), `NetSession`, `Messages<NetEvent>` and two
  `Update` bridge systems (each `run_if` its transport resource exists) →
  zero cost while `Idle`. FSM factored into the pure `next_status(role,
  status, trigger)` + `NetSession::apply`. **TDD**: 16 tests written first —
  RED (all panic on the `todo!()` stub) → table implemented → GREEN. 17
  tests total: 10 pure transition-table tests (both roles, every state,
  Stop-from-every-state, illegal-trigger ignores), a watchdog test, an
  occupied-port `BindFailed`-no-panic test, a version-mismatch kick test
  (via renet's `new_local_client` seam — no UDP), and the two-`App`
  loopback connect/`Ready`/`Bye` + graceful-exit tests. The loopback test
  is inherently GREEN-after-implementation (transport wiring can't fail
  before it exists) — documented in the test header. **Observer
  consumption (Bevy 0.19)**: `app.add_observer(fn ev: On<RenetServerEvent>)`,
  patterns via `**event` (`On` → `RenetServerEvent` → `Deref` →
  `renet::ServerEvent`); observers can't write `Messages`, so they buffer
  into a private `NetSession::server_notes` queue that the host system
  drains (same frame). `NetcodeErrorEvent` deliberately unobserved —
  unobserved triggers are no-ops, and the post-disconnect error spam is
  silenced by dropping transport resources the same frame. Client side
  polls `is_connected`/`is_disconnected` +
  `NetcodeClientTransport::disconnect_reason()` instead of the built-in
  `client_just_*` predicates (their `Local<bool>` is one-shot per schedule
  — edge detection done manually). **API surprises found**: (1) renet_netcode
  re-exports the netcode reason under the alias itself — import
  `bevy_renet::netcode::NetcodeDisconnectReason`, the base name fails; (2)
  that enum has 7 variants (N1's notes omit `ConnectionResponseTimedOut` —
  mapped to `Timeout`); (3) **host-side loss reasons are flattened**: renet's
  `remove_connection` (`server.rs:129-134`) emits
  `ClientDisconnected{reason: Transport}` for *any* netcode-originated loss
  (clean disconnect packet and 15 s timeout alike), so the host sees
  `Lost(Transport)` for "peer vanished" — the guest keeps full granularity
  (`Denied`/`Timeout`/…); escape hatch if N5 wants the split host-side:
  `NetcodeServerTransport::time_since_last_received_packet`; (4)
  `RenetClient` constructs in `Connecting` — no false disconnect on join;
  (5) dropping a peer `App` without `net_stop()` sends nothing (survivor
  waits the full 15 s) — all exit paths must call `net_stop()` (graceful,
  ~1 frame; loopback-tested). Guest FSM clarification: guest passes through
  `Ready` (`Handshaking` = Hello queued, `Ready` = flushed/awaiting
  `MatchStart`) so both peers read `Ready` after handshake and N5's
  "waiting-for-host" line exists. **Handoff boundary for N3**: N2's systems
  drain the renet channels only during the handshake window (host
  `Handshaking`, guest `Handshaking|Ready`) — from `InMatch` on N3 owns
  every message incl. `Bye` (documented at the top of `session.rs`). Test
  port: `TETRIS_TEST_NET_PORT` (deterministic default 34857),
  `TEST_NET_LOCK` static serializes all socket tests in-binary (N6 must
  share it); foreign-collision caveat documented. Gates: fmt, clippy `-D`,
  `cargo test --workspace` (163 app + 136 core + 6 integration).
- **files edited/created**: `crates/tetris-app/src/core_bridge/net/session.rs`
  (new — session FSM, plugin, free fns, bridge systems, tests),
  `crates/tetris-app/src/core_bridge/net/mod.rs` (`mod session;` + re-export),
  `crates/tetris-app/src/core_bridge/mod.rs` (`NetPlugin` mount),
  `netplay-plan.md` (this entry)

### N3: Lockstep engine (tick clock, input delay, mirror stepping, desync check)
- **location**: `core_bridge/net/mod.rs` (submodule decl),
  `core_bridge/net/lockstep.rs` (new)
- **description**: The tick driver for `NetStatus::InMatch`, on `FixedUpdate`
  **in place of** the versus bridge's direct drain (N4 installs the gate;
  here, provide systems + `NetLockstep` resource: per-side pending-input ring
  `VecDeque<(tick, Vec<Action>)>`, host's `remote_inputs` fed from
  `TickInput`, `tick: u64` host counter / guest next-expected-tick + a ≤D
  batch buffer). Delay from `MatchStart.match_delay` (env default 8 via
  `TETRIS_NET_DELAY`, clamp 2..=30 — `SEED_ENV` env pattern).
  Ordering **must mirror `versus_bridge_system`**: apply actions, then
  `tick(left)`, `tick(right)`; emit `VersusEvent` messages for every
  `MatchEvent` produced, so T26 juice/audio/HUD work unmodified.
  Host per step: build `TickBatch {tick, left, right}` from the due
  delayed-queue + arrived remote inputs (missing remote ⇒ empty list, count
  a `dropped_late_inputs` diagnostic), apply to local `Match`, send batch,
  bump tick. Guest per step: if batch for expected tick buffered → apply
  (actions then tick both sides), else stall (reliability guarantees eventual
  arrival; count a `stall_steps` diagnostic). Both sides emit
  `TickInput {tick, actions}` for their local side's queued actions.
  Every 60 ticks both send `SnapshotHash` (host+guest full-match hash via
  `snapshot_hash(match_.snapshot())`); mismatch → `NetEvent::Desync` and the
  **teardown contract below**. **Gates**: skip when `SimPaused.0` or
  `*app_state != Playing` — this is what makes the Lost/Desync freeze real
  (N5 flips `SimPaused` and shows the overlay; the lockstep then simply
  stops ticking while `VersusMatch.active` stays true so the frozen boards
  remain rendered under the overlay). **Teardown contract** (both desync and
  `Lost`/`Bye`): (1) set `SimPaused`, show the net overlay root with an
  **explicit high `ZIndex`** (documented regressions at `screens_menu.rs:960`
  and `:2055` show why); (2) the match stays `active` (frozen boards visible
  behind the overlay, versus HUD must not swallow the overlay's buttons —
  cover with a click-ability test); (3) the user's "Back to title" click runs
  an `end_versus`-style full teardown (`active=false`, roots restored to the
  clean-solo regime, `NetSession → Idle`).
- **validation**: fake-transport seam (`NetOut` trait, N3-owned) running both
  "peers" deterministically in one process: delay math (input at t batched at
  t+D), empty-tick correctness, guest stall-and-recover on artificially
  delayed batch delivery, late-input drop accounting, desync detection on a
  deliberately forked mirror, teardown contract checks (SimPaused honored, no
  tick progress while paused); **proptest**: two `Match`es fed the same
  `TickBatch` stream are snapshot-identical at every tick. All existing tests
  green; clippy/fmt clean.
- **status**: Completed
- **log**: `NetLockstep` resource + pure state machine in `lockstep.rs`
  (`reset_for_match`, `schedule_local`, `ingest`, `step_host`, `step_guest`),
  every step method taking `&mut Match` + a `&mut dyn NetOut` so production
  logic runs verbatim in-process over the fake transport. `NetOut` trait seam
  (send-only) with `RenetServerOut`/`RenetClientOut` renet adapters; receive
  via `server_inbox`/`client_inbox` (poll both channels, decode, never panic
  on hostile bytes) → `ingest`. `wire_channel()`: Hello/MatchStart/TickInput/
  TickBatch on ReliableOrdered, SnapshotHash/Bye on ReliableUnordered. Two
  `FixedUpdate` systems (`net_lockstep_host_system`,
  `net_lockstep_guest_system`) mounted by `NetLockstepPlugin` from
  `NetPlugin::build()`; all params but `NetLockstep` optional → inert in
  non-netplay apps (`add_message::<VersusEvent>` re-registration is
  idempotent, Bevy 0.19 `contains_resource` guard). **Stepping order**
  (`apply_batch`, the single path host and mirror share — proptest-pinned):
  apply left actions → `tick(Left)` → apply right → `tick(Right)`, mirroring
  `versus_bridge_system` (`versus.rs:255-264`); systems write `VersusEvent`
  per `MatchEvent`, bump `VersusMatch::steps`, and crown `VersusWinner`
  once (`winner.0.is_none()` guard — `start_versus` clears it). **N4 gating
  contract**: while `InMatch`, `versus_bridge_system` must skip its *entire*
  drain-then-tick block incl. its own steps/crowning (double-stepping
  corrupts the mirror) and pin `.after(versus_bot_system)` (private to
  `versus.rs`) so Bot-seat pushes schedule with delay like human actions.
  **Delay**: local actions schedule for `tick + D` AND emit `TickInput`
  immediately; both peers emit; the guest discards the host's `TickInput`
  echo and the host discards stray `TickBatch` via a `role` field the
  systems mirror from `NetSession` — a late `TickInput` whose target already
  executed is dropped and counted in `dropped_late_inputs`; the guest mirror
  applies only batch contents (guest's own ring is bookkeeping), so late
  input can never fork the boards. **Stall policy**: host sends a batch
  every tick (empty lists included — clock pacing); guest executes strict
  in-order from a sorted `batch_buffer`, counting `stall_steps`. **Desync
  check**: every `HASH_CHECK_PERIOD` (60) executed ticks (labels 59, 119, …)
  both sides send `SnapshotHash { side, tick, left: h, right: h }` — both
  fields carry the full-match `snapshot_hash` (N1 codec exposes only the
  whole-`MatchSnapshot` hash; per-side granularity deferred — worth a plan
  amendment if N6 wants per-side localization); rolling 8-entry windows per
  direction, mismatch on a common label → `LockstepSignal::Desync{tick}` +
  windows cleared (no repeat-fire). **Gates** (freeze mechanism): skip while
  `SimPaused.0` / `AppState != Playing` / no transport (host also needs
  `session.peer`) / `!VersusMatch::active`; paused skip holds the
  `VersusActions` queues, inactive-match skip clears them. **Teardown
  contract shipped**: Desync/Bye signals set `SimPaused` and write
  `NetEvent::Desync`/`ByeReceived` (Bye also drives `NetTrigger::Bye` →
  `Lost(PeerDisconnected)`; `NetSession::apply` opened to `pub(crate)`);
  the match stays `active` so frozen boards keep rendering; `net_freeze` +
  `net_leave_to_title` (graceful `net_stop` + reset + un-pause +
  `end_versus`) and `NET_OVERLAY_ZINDEX` (`ZIndex(100)`, strictly above HUD
  0 / submenu 1 per the recorded click-swallow regressions) are N5's hooks.
  A mid-match `MatchStart` parks in `pending_start` for N4's rematch.
  `net_stop` fixed to remove `RenetServer`/`NetcodeServerTransport`
  independently (previously a transport-less server survived → not `Idle`).
  **Validation** — 26 tests (app suite 163 → 189): 13 pure fake-transport
  (`NetSim`: latency-controlled in-memory links, real encoded `NetMsg`s)
  covering delay math, empty ticks, stall-and-recover, late-input accounting,
  batch merge, 60-tick hash exchange, forked-mirror desync detection on both
  peers, `Bye` signal, `pending_start` parking, stale-batch ignore; a
  256-case proptest pinning the mirror invariant (same batch stream ⇒
  snapshot- and hash-identical) and cross-checking
  `snapshot_hash` ⇔ `snapshot` equality; 9 Bevy-system tests over renet's
  `new_local_client` seam (no UDP) covering inert-while-not-InMatch, host
  pacing + `TickInput(t+D)` + no early application, `SimPaused` hold/
  resume, non-`Playing` freeze, inactive-match guard, desync freeze +
  `net_leave_to_title` full teardown, wire `Bye`, winner crowning through
  the lockstep, guest drain/stall/recovery; plus a thin real-UDP loopback
  smoke (port 0 — no fixed-port lock needed) round-tripping a guest drop
  `TickInput` → host batch → guest mirror with hash-equal boards.
  RED→GREEN note: tests and impl were authored in one pass against the
  planned API; the first wired `cargo check --all-targets` and test run
  failed (transport accumulator bug in the fake seam, guest-role echo
  handling, dial-vs-bind address mismatch in the loopback test), all fixed
  to GREEN — the fake seam demonstrably catches real wiring faults.
  **API surprises**: (1) `SnapshotHash` carries `side` (N1 notes underplay
  it) but the hash is whole-match; (2) guest must NOT reset after the
  session flips `InMatch` — `FixedUpdate` runs after the `Update` bridge in
  the same frame, so `MatchStart` + first batches can already have executed
  (loopback test resets before the host starts the clock — N4: reset the
  guest's lockstep when handling `MatchStart`, i.e. *before* the next fixed
  step); (3) `AppState::Playing` is the default state, so netplay gating is
  live from the first frame; (4) 1.95 clippy: `is_multiple_of` over `%`,
  `too_many_arguments` allow on the step systems (repo convention).
  `TETRIS_NET_DELAY` desire flows through N2's negotiation; `reset_for_match`
  re-clamps defensively. Gates: fmt, clippy `-D`, `cargo test --workspace`
  (189 app + 136 core + 6 integration).
- **files edited/created**: `crates/tetris-app/src/core_bridge/net/lockstep.rs`
  (new — state machine, `NetOut` seam, systems, teardown contract, tests),
  `crates/tetris-app/src/core_bridge/net/mod.rs` (`mod lockstep;`),
  `crates/tetris-app/src/core_bridge/net/session.rs` (`apply` → `pub(crate)`,
  `NetLockstepPlugin` mount, `net_stop` independent-removal fix),
  `crates/tetris-app/Cargo.toml` (+`Cargo.lock`) (proptest dev-dep),
  `netplay-plan.md` (this entry)

### N4: Versus bridge + input integration (`Controller::Net`)
- **depends_on**: [N3]
- **location**: `core_bridge/versus.rs`, `input.rs`,
  `screens_menu.rs` (**one-line carve-out only**, see item 3)
- **description**:
  1. Extend `Controller` with `Net` (additive variant).
  2. **Cross-file compile carve-out**: `winner_text` (`screens_menu.rs:275`)
     exhaustively matches `Controller`, so N4 adds a minimal `Net` arm
     ("OPPONENT" label) in `screens_menu.rs` — N5 refines it. (Documented
     exception to N5's file ownership; waves are sequential anyway.)
  3. Gate `versus_bridge_system`: when `NetSession.status == InMatch`, skip
     the direct `Match::apply`/`tick` drain — N3's lockstep owns stepping —
     while keeping `VersusWinner` surfacing and `steps` bookkeeping alive.
     Suppress R-restart in **`versus_restart_on_r_system`** (that's the
     actual `Update`-set R handler, not the bridge drain) during `InMatch` —
     a local reseed would destroy mirror sync.
  4. `versus_bot_system`: unchanged. A `Bot` side pushing its own side's
     local queue transparently feeds the net input path — zero
     special-casing, and the mechanism N6's bot-vs-bot relies on.
  5. Input routing in `versus_input_system`: while `InMatch`, only the
     local side's queue is fed from keys (host: left/P1 preset; guest:
     right/P2 preset); the remote side's queue is never touched by local
     keys. **Binding-preset fix**: the lone-human override
     (`input.rs:750`, `lone_human_p1 = p1_human && !p2_human`) currently
     fires for the host seat pair `Human+Net` and would hand the host the
     arrow/solo-alternate preset; treat a `Net` seat as occupied
     (`!(p2_human || p2_net)`), so host gets `bindings.p1` (WASD) and guest
     gets `bindings.p2` (arrows) via the normal two-seat path. Preserves all
     existing local-versus behavior (Net never appears locally). Not fully
     "additive" — a deliberate behavior adjustment scoped to `Net` seats;
     pinned by tests.
  6. `start_net_match(rule, local_side, seed, delay)` / `end_net_match`
     free functions mirroring the `start_versus`/`end_versus` pattern:
     host honors `SEED_ENV` for harness runs else wall clock, constructs the
     `Match` locally AND sends `MatchStart`; the guest constructs its
     `Match` **only** from the received `MatchStart` seed/rule/delay — never
     a locally derived seed.
- **validation**: all existing tests green unmodified; new gate-matrix tests
  (net Idle → bridge path byte-identical to today; InMatch → drain skipped,
  winner surfacing alive, R suppressed); preset tests: host seat pair
  `Human+Net` drives left via P1 bindings, guest seat pair `Net+Human` via
  P2 bindings, remote queue untouched by local keys; lone-human local-versus
  behavior regression test (vs `Bot` still gets the arrow+solo-alternate
  preset); `start_net_match` seed-propagation test (guest `Match` seed ==
  host's). clippy/fmt clean.
- **status**: Completed
- **log**: `Controller::Net` added (additive). No existing test was modified —
  the only cross-file compile carve-out was `winner_text`
  (`screens_menu.rs:277`), which gets a minimal
  `Controller::Net => "OPPONENT WINS"` arm (+comment line); **N5 refines this
  arm's copy** (it is documented as N5-owned copy, this is the N4→N5 handoff).
  **Bridge gating**: `versus_bridge_system` gains `Option<Res<NetSession>>` and
  early-returns on `status == InMatch` *before* the `!active` queue clear — the
  lockstep systems own the queues, `steps` and crowning in `InMatch` (no
  double-step). With the session `Idle`/absent the path is byte-identical
  (`net_idle_leaves_the_bridge_drain_path_byte_identical` asserts the stepped
  snapshot equals a hand-advanced `Match`). `versus_restart_on_r_system` gains
  the same optional param and is inert in `InMatch` (local reseed risk).
  `versus_bot_system` **unchanged** — a `Bot` local seat's pushes schedule with
  delay like human actions. **Ordering pinned** in `VersusBridgePlugin`:
  `versus_bot_system.before(net_lockstep_host_system).before(net_lockstep_
  guest_system)` — the lockstep fns appear only in `.before()` (records an edge,
  never re-adds them; no-op when a plugin is mounted without the other), and the
  bridge keeps its existing `.after(versus_bot_system)`.
  **Input** (`input.rs`): lone-human override now `p1_human && !(p2_human ||
  p2_net)`, so a `Net` seat counts as occupied. This is the deliberate,
  `Net`-scoped behavior adjustment (not pure additive) the plan called for; the
  RED→GREEN evidence is `net_host_seat_pair_uses_p1_wasd_and_never_the_remote_
  queue` (fails before the fix: the host took the arrow preset). The two-seat
  configuration N5 arms (`Human+Net` host / `Net+Human` guest) *is* the routing:
  a `Net` seat gets `out=None` in `drive_versus_side`, so local keys can never
  reach the remote queue — no role branch needed in `versus_input_system`.
  `Human+Bot` lone-human behavior untouched (regression test added:
  `lone_human_vs_bot_still_gets_arrow_and_solo_alternate_preset`).
  **Lifecycle** (`versus.rs`, `&mut World` free fns): `start_net_match(world,
  rule, local_side, seed, delay)` — host honors `SEED_ENV` (`env_seed().
  unwrap_or(seed)`) for harness runs else the caller's `seed` (N5 = wall clock,
  N6 = fixed), flips `Ready→InMatch` via `enter_match()`, `reset_for_match`,
  builds its mirror, and sends `MatchStart{seed,rule,match_delay}` via
  `RenetServerOut`. Guest: builds its mirror **only** from `MatchStart`
  (never a local seed) — `end_net_match(world)` = lockstep reset + `end_versus`
  (leaves transport/session alone; N5's Back-to-title uses `net_leave_to_title`).
  **N4 contract hook (session.rs, recorded)**: `guest_net_system` gained a
  `ResMut<NetLockstep>` param and now parks the *initial* `MatchStart`
  (`Handshaking|Ready`) into `pending_start` alongside the status flip, so a
  single consumer (`guest_pending_start_system`, Update, guest-only) rebuilds the
  mirror identically for first start and mid-match rematches. The host never
  consumes `pending_start` (shields it from a hostile mid-match `MatchStart`).
  **Seed-propagation test** = real-UDP two-app loopback (`start_net_match_
  propagates_the_seed_to_the_guest_mirror`, port 0 like N3's): host starts via
  `start_net_match`, guest mirrors purely off the wire, asserts
  `guest.seed == host.seed` (a wall-clock seed could never equal the fixed one)
  and hash-equal mirrors after 30 shared lockstep ticks. GREEN-after-
  implementation (the wiring can't fail before it exists); the *gating*
  correctness is non-vacuously proven by that loopback staying hash-equal (an
  ungated bridge would fork it) and by the RED preset test. **Winner surfacing
  in `InMatch` is alive via the lockstep** (N3's `winner_surfaces_through_lockstep`)
  — N4's gate defers crowning to it (verified: the gated bridge writes neither
  `VersusWinner` nor `steps` in `InMatch`). **Gates**: fmt, clippy `-D` clean;
  `cargo test --workspace` = 197 app (+8 new) + 136 core + 6 integration, all
  existing tests green UNMODIFIED.
- **files edited/created**: `crates/tetris-app/src/core_bridge/versus.rs`
  (`Controller::Net`, InMatch gating of the bridge + R handler, lockstep
  ordering pin, `start_net_match`/`end_net_match`/`setup_net_mirror`/
  `guest_pending_start_system`, gate-matrix + lifecycle + loopback tests),
  `crates/tetris-app/src/input.rs` (lone-human `Net`-seat fix + preset tests),
  `crates/tetris-app/src/screens_menu.rs` (**one carve-out**: `winner_text`
  `Controller::Net` arm), `crates/tetris-app/src/core_bridge/net/session.rs`
  (N4 contract hook: park the initial guest `MatchStart` in `pending_start`),
  `netplay-plan.md` (this entry).
- **N5 handoff** (see also N3's `net_leave_to_title`/`NET_OVERLAY_ZINDEX`):
  - `winner_text` Net arm currently returns `"OPPONENT WINS"` (both roles).
    Refine as needed; `winner_text(winner, p1, p2)` already carries both seats.
  - `start_net_match(world: &mut World, rule: AttackRule, local_side: Side,
    seed: u64, delay: u8)` and `end_net_match(world: &mut World)`. N5's Host
    "start" button: call `start_net_match(world, rule, Side::Left,
    wall_clock_seed(), session.input_delay)` (it flips the session itself; it
    honors `TETRIS_SEED` when set, so leave the `seed` arg to the caller for
    real play and let the harness pin it). Guest needs nothing — the mirror is
    built from `MatchStart`.
  - **Rematch**: host calls `start_net_match` again (re-arms its own mirror and
    resends `MatchStart`); the guest auto-rebuilds via `pending_start` →
    `guest_pending_start_system` within ≤1 frame, resetting to the new seed at
    tick 0. Do NOT route guest Rematch through `start_versus`/`start_net_match`
    locally (guest must never reseed locally), and R is already suppressed in
    `InMatch`. Local-seat controllers (`Human` for a duel) are preserved across
    rebuild; the remote seat is always forced to `Controller::Net`.
  - N6 bot-vs-bot-across-the-wire: arm each peer's *local* seat to `Bot`
    (host `Bot+Net`, guest `Net+Bot`) — `versus_bot_system` pushes the local
    queue and the lockstep transports it with zero special-casing. The local
    controller follows `NetSession::role`, not a `start_net_match` arg.

### N5: Online menu flow, IP entry, status & error overlays, exits
- **depends_on**: [N4]
- **location**: `screens_menu.rs` (also refines the `Net` arm in
  `winner_text`), `settings_persist.rs` (additive helpers for a separate
  `net_profile` file)
- **description**: Extend the `VersusFlow` stage machine with an
  `OnlineFlow`: Title → "1 v 1" gains a Local/Online axis; Online →
  **Host** (status `Listening`, shows port + connect hint; local-IPv4 hint
  via the connect-to-public `UdpSocket` trick **with a fallback string** when
  no route exists; "waiting for challenger…" on `Ready`; **Esc here calls
  `net_stop()`** — un-listens and frees the port, then back) or **Join**
  (text-entry widget: charset digits/dots/colon, backspace, Enter submits;
  Bevy 0.19 clipboard paste only if it works without touching `main.rs`, else
  keyboard-only — verify and log); status line per `NetStatus`
  (connecting/handshaking/ready/waiting-for-host — the last covers the
  guest-at-`Ready`-before-host-rule-pick gap). Host rule/opponent picker
  (rule picker reused; right side forced `Net`) → `start_net_match` →
  `MatchStart`. Overlays (winner-overlay spawn/visibility patterns, N3
  teardown contract, explicit `ZIndex`): `BindFailed` ("port in use"),
  `VersionMismatch`, `JoinTimeout`/`PeerLost` (frozen boards behind,
  "Connection lost — host offline or match full" for timeouts),
  `Desync {tick}` ("desync at tick N — match aborted"), `ByeReceived`
  ("opponent left"). **Mid-match exit**: Esc during `InMatch` opens a
  confirm overlay ("Leave match?" Leave/Stay) instead of the pause chord —
  **pause chord gated off in `pause_chord_system` for `InMatch`** (lockstep
  has no authoritative pause); Leave sends `Bye` + teardown. **Rematch**:
  host-only (role-gate the `VersusRematchButton` for guests) and **routed
  through `start_net_match` (fresh `MatchStart`), never `start_versus`** —
  the local reseed in `start_versus` would fork the guest's mirror. Guest
  side: the rematch button is hidden; the guest waits for the host's
  `MatchStart`. Persist `net_profile.json` (last join address, prefilled in
  the entry) via `settings_persist` helpers as a **separate** resource —
  `Settings` in `state.rs` must not be reshaped (T1 contract,
  `state.rs:1-3`). Esc walks OnlineFlow stages; entering Online never
  disturbs solo/local-versus flows (their stage tests pass untouched).
- **validation**: headless-App menu-flow tests per stage transition
  (mirroring T26's flow tests); `winner_text` Net-arm test; overlay
  trigger-and-teardown tests per `NetEvent` variant, incl. the recorded
  visibility/click regressions' patterns (overlay buttons clickable while the
  frozen versus HUD is visible); entry edit-logic unit tests (charset,
  backspace, IPv4:port accept/reject); `net_profile` round-trip test. Author
  sign-off gate (author's rule: agent runs the checklist, posts, stops):
  two machines play a full garbage match over LAN + internet; default delay
  feels responsive. clippy/fmt/test gates green.
- **status**: Completed
- **log**:
  - Keyboard-only IP:port entry as specified. Clipboard finding: **Bevy 0.19
    removed clipboard support entirely** (no `clipboard` surface anywhere in
    `bevy_window`/`bevy_winit` 0.19.1 — verified against the vendored
    sources), so paste is unavailable with or without `main.rs`; the entry is
    keyboard-only by platform, not by shortcut. Charset digits/dots/colon
    (Shift+`;` → `:`), Backspace/Delete, Enter/NumpadEnter submit, 44-char
    cap; `parse_join_addr` accepts trimmed `IPv4:port` with port ≠ 0.
  - Local/Online axis realized as a sibling Title **"Online"** entry (marker
    `OnlineButton`, spawned in `build_menu_ui`, owned panel flow in
    `online_ui.rs`) rather than a stage inside the 1v1 submenu — every
    existing T26 `VersusFlow` stage test passes untouched, satisfying the
    "never disturbs solo/local-versus flows" contract.
  - Wiring gates in `screens_menu.rs`: `pause_chord_system` returns early
    while `NetStatus::InMatch` (Esc belongs to the leave-confirm; lockstep
    has no authoritative pause); `versus_button_clicks` skips Rematch/Menu
    while `InMatch` (host re-arms through `start_net_match` — never the
    local-reseed `start_versus` — Menu runs `net_leave_to_title`); guest
    `VersusRematchButton` hidden (role gate); winner headline role-aware via
    `net_winner_text` once a seat is `Controller::Net`; `TitleRoot` hidden
    while the online flow is open (same discipline as the 1v1 submenus,
    per the recorded click-swallow regressions).
  - `OnlineUiPlugin` mounts from `MenuScreensPlugin::build` (single canonical
    mount). Overlay roots carry explicit `NET_OVERLAY_ZINDEX` so their
    buttons pick over the frozen-but-visible versus HUD (teardown contract
    respected: `SimPaused` held, `VersusMatch` stays active until Back to
    title runs `net_leave_to_title`).
  - `net_profile.json` is a **separate** resource (`NetProfile`) with its own
    load/flush/exit systems chained beside the settings ones; `Settings` in
    `state.rs` untouched (T1). Last-submitted join address persists and
    prefills the entry.
  - Cross-module seam: the screens wiring names `crate::core_bridge::net::*`
    (the `mod net;`-private blocker posted to the board); unblocked by N6's
    one-line `pub(crate) mod net;` in `core_bridge/mod.rs`.
  - Tests: 31 `online_ui` (14 pure entry/status/text logic + 17 headless
    flow/overlay/teardown) and 4 new `screens_menu` (Title button stage
    transitions with title-hide, no-cross-talk with solo/versus, role-aware
    headline ×4 role/winner combos, local headline unchanged); existing T26
    suite passes unmodified. **Pending author sign-off**: two-machine LAN +
    internet playtest and delay-feel check (an agent cannot operate two
    machines; request: host on machine A, join B→A over LAN for one full
    garbage match, repeat over internet, confirm default `TETRIS_NET_DELAY`
    feels responsive, plus mid-match leave from each end).
- **files edited/created**: `crates/tetris-app/src/core_bridge/net/online_ui.rs`
  (new — flow machine, entry, overlays, session watcher, mountable plugin),
  `crates/tetris-app/src/screens_menu.rs` (Online title entry, `InMatch`
  pause/rematch/menu gates, role-aware headline, title-hide, plugin mount),
  `crates/tetris-app/src/settings_persist.rs` (`NetProfile` +
  load/flush/exit helpers + tests)

### N6: Cross-process net harness + CI-automatable end-to-end test
- **depends_on**: [N4]
- **location**: `core_bridge/net/harness.rs` (new, production env-var path +
  `#[cfg(test)]` E2E), `core_bridge/net/mod.rs`, `core_bridge/mod.rs`
  (harness startup hook like `ONE_V_ONE_ENV`), `.github/workflows/ci.yml`
- **description**: Two deliverables, because CI **cannot run the real
  binary** (verified: `main()` unconditionally adds `DefaultPlugins`; no
  display on `ubuntu-latest`; `CARGO_BIN_EXE` spawns would crash on winit —
  an xvfb full-binary CI job is explicitly out of scope for v1, logged as a
  follow-up candidate):
  1. **CI E2E test** (`#[cfg(test)]` in `harness.rs`): two `MinimalPlugins`
     apps in one process over **real renet/netcode UDP on loopback** — real
     session FSM, real lockstep driver, real `Match`es, host side driven by
     `versus_bot_system` (`Bot` for left), guest side by a `Bot` for right —
     a full garbage match to a crowned winner, asserting both sides'
     per-60-tick `SnapshotHash` streams equal throughout and final snapshots
     equal, both apps shut down cleanly. Plus a `TETRIS_NET_FORK=guest:<tick>`
     test-only lockstep hook: one flipped input action at a tick → the
     equality assertion **must** fire (proves the test isn't vacuous).
     Port strategy per N2's fixed-test-port rule; mark
     `#[ignore]`-parallel-safe (serial with the N2 socket test if needed).
  2. **Desktop manual harness** `TETRIS_NET=host:<port>` /
     `TETRIS_NET=join:<addr>` env modes mirroring `TETRIS_1V1` (full
     `DefaultPlugins` app, human-run only): same bot-vs-bot-across-the-wire,
     logs `NET match_done seed=… ticks=…`, `NET final_hash left=… right=…`,
     exits after 2 matches (garbage, race); any `Desync`/`Lost`/120 s stall
     → exit 1. Used for the N5 sign-off gate and real-NAT testing.
  CI: the E2E rides the existing `cargo test --workspace` job (no new job,
  no xvfb); add it to the nightly soak list note if flaky-skip is ever needed.
- **validation**: locally: `cargo test -p tetris-app net::harness` green,
  fork-injection variant fails as designed; `TETRIS_NET` host+join on one
  machine via loopback complete 2 matches with identical `final_hash` and
   exit 0; on two machines over the LAN the same holds (author runs).
- **status**: Completed
- **log**: Commits `3581a62` + `668a530`. **CI E2E**: 2 non-ignored tests
  (`harness.rs`) — two `MinimalPlugins` apps over **real renet/netcode UDP on
  loopback, OS-assigned port 0** (no contention): bot-vs-bot Garbage to a
  crowned winner with both peers' per-60-tick `SnapshotHash` streams equal
  throughout + final snapshots equal; and fork injection
  (`TETRIS_NET_FORK=guest:<tick>` hook) → `Desync{119}` on **both** peers +
  `SimPaused` freeze + provably divergent independently-recorded streams
  (non-vacuity). Rides existing `cargo test --workspace`; no ci.yml change
  needed. **Desktop harness**: `TETRIS_NET=host:<port>` / `join:<ip>:<port>`
  bot-vs-bot (Garbage → Race{40}), `NET match_start/match_done/final_hash/
  complete/fail` logging, exit 1 on desync/stall/loss, exit 0 on success;
  startup hook in `core_bridge/mod.rs` mirrors `ONE_V_ONE_ENV`. **LIVE
  two-process X11+NVIDIA verification**: identical winner/ticks/final_hash on
  both peers across 2 matches (clean, exit 0 both sides); fork run → "desync
  detected at tick 119 — match frozen", both processes exit 1. Three harness
  lifecycle bugs found only by running live: guest rematch counting re-keyed
  by match seed; phantom 0-tick host rematch log; `std::process::exit`
  mid-frame SEGFAULT on winit/GPU atexit → success uses `AppExit::Success`,
  failure `libc::_exit(1)` on unix. Also landed the `pub(crate) mod net;`
  enablement N5 needed. **Routed follow-ups** (beyond N6 ownership): (1) real
  production bug — `guest_net_system` pre-match drain reads the whole reliable
  channel on the `InMatch`-transition frame and discards `TickBatch`es queued
  behind `MatchStart` → guest stalls at tick 0 under load; harness holds
  `SimPaused` briefly after `MatchStart` as a workaround — root fix done as a
  post-N6 fixup (see below); (2) systemic test flake: N2/N3/N4/N5 UDP tests
  sharing `TETRIS_TEST_NET_PORT` contend in parallel (`--test-threads=1`
  clean) — port-0 bind + read-back fixup. Desktop-harness
  `MATCH_START_HOLD` revisited once the drain bug is fixed.
- **files edited/created**: `crates/tetris-app/src/core_bridge/net/harness.rs`,
  `crates/tetris-app/src/core_bridge/mod.rs`
- **post-N6 fixup** (`f9d6d51`): drain bug root-fixed — `guest_net_system`
  re-checks status each drain iteration (`while matches!(status,
  Handshaking|Ready)`), never consuming past the `InMatch` transition;
  symmetric guard added to the host `Hello` drain; consume-on-read rule in
  module docs. Deterministic regression test
  `matchstart_transition_frame_does_not_swallow_queued_tickbatches`
  (RED: guest tick-0 stall; GREEN: all queued batches replayed in order,
  snapshot-equal). `MATCH_START_HOLD` removed; E2Es start live (every run
  crosses the old stall window). UDP tests moved to port-0 + read-back
  (`TETRIS_TEST_NET_PORT` deleted; `TEST_NET_LOCK` kept for the occupied-port
  probe). 3× parallel + serial full-suite green; systemic flake resolved.

### N7: Netplay soak + protocol-robustness audit
- **depends_on**: [N6]
- **location**: `core_bridge/net/harness.rs` (soak extension),
  `core_bridge/net/protocol.rs` (fuzz tests), `netplay-plan.md` (audit log)
- **description**: No FP-risk audit needed (core verified integer-only).
  Instead: (a) extend the E2E into a 20-match soak (alternating
  Garbage/Race, seed sweep, one very long Race-to-40 with garbage-storm
  `MAX_GARBAGE_PER_LAND` churn), diffing per-tick hash streams across the
  two in-process mirrors — end-to-end validation of serialization + plumbing +
  lockstep under sustained load, tagged `#[ignore]` for the nightly job like
  `tests/soak.rs`; (b) proptest byte-fuzz over `protocol::decode`: no panic,
  only `ProtocolError` (host must never fall over on hostile bytes — relevant
  because auth is Unsecure); (c) record the audit result in this plan's log,
  including the two-machine desktop `TETRIS_NET` soak if the author ran it.
- **validation**: 20-match soak green locally and on CI nightly
  (`--ignored`), zero hash divergence over all matches; 10k-case decode fuzz
  with no panic; audit conclusion written here.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### N8: Docs, PRD/README/CHANGELOG
- **depends_on**: [N5, N6]
- **location**: `README.md`, `PRD.md`, `CHANGELOG.md`
- **description**: README: Features bullet (online 1v1 lockstep, delay-based),
  Controls note (host = left/P1 preset, guest = right/P2 preset; no pause in
  net matches, Esc = leave-with-confirm), env-var table rows (`TETRIS_NET`,
  `TETRIS_NET_DELAY`, `TETRIS_NET_FORK`), "Playing online" section
  (port-forward/NAT caveat + firewall note, "match is open while listening —
  keep sessions short" warning). PRD: §4 multiplayer non-goal struck →
  reference the implemented §15 scope; §15 marked for local+online versus,
  keeping lobby/relay/discovery/rollback/session-tokens as post-v1.
  CHANGELOG unreleased entry; tetris-plan.md note that T24–T26 + N1–N8 form
  netplay v0.1.
- **validation**: prose review; docs-only diff; links/anchors resolve.
- **status**: Completed
- **log**: Docs-only commit `72baf12`. README: intro + Features bullet for
  local/online versus; Controls blockquote (host/left = P1 WASD, guest/right
  = P2 arrows; no pause in net matches, Esc = leave-with-confirm); "Playing
  online" section (Host/Join flow, UDP port shown on Host screen,
  keyboard-only `ip:port` entry — Bevy 0.19 has no clipboard API, D=max delay
  ≈133 ms, NAT port-forward + firewall caveat, "port is open while listening"
  warning, no-lobby/relay note with the "host offline or match full" wording,
  version-handshake refusal, desync freeze); env table rows for `TETRIS_NET`,
  `TETRIS_NET_DELAY` (default 8, clamp 2..=30), `TETRIS_NET_FORK` — all
  fact-checked against code (harness.rs:87/91, session.rs:104-113/623,
  online_ui.rs). PRD: §4 multiplayer non-goal struck → §15; §15 retitled
  "Versus (netplay v0.1) & Future Extensions" splitting implemented
  local+online versus from post-v1 (lobby/relay/discovery, rollback, session
  tokens stay non-goals); §1/§13 pointers updated. CHANGELOG `[Unreleased]`
  entry: netplay v0.1 = T24–T26 + N1–N8. tetris-plan.md: netplay v0.1 note.
  Doc drift found+fixed en route: N5's "44-char cap" claim (code: 45);
  stale pre-versus README/PRD claims. Author sign-off (two-machine LAN/
  internet, delay feel) still open — checklist in N5 log.
- **files edited/created**: `README.md`, `PRD.md`, `CHANGELOG.md`,
  `tetris-plan.md`

## Parallel Execution Groups

| Wave | Tasks | Can Start When |
| --- | --- | --- |
| 1 | N1 | Immediately |
| 2 | N2 | N1 |
| 3 | N3 | N2 |
| 4 | N4 | N3 |
| 5 | N5, N6 | N4 (disjoint files after the N4→N5 `winner_text` handoff: `screens_menu.rs` vs `net/harness.rs` + `core_bridge/mod.rs` hook + CI) |
| 6 | N7, N8 | N6 / N5+N6 |

Netcode is inherently sequential in the core path (N1→N4); the only safe
parallelism is N5∥N6 (wave 5) and N7∥N8 (wave 6).

## Testing Strategy

- **Protocol**: round-trip + malformed-decode unit tests (N1); byte-level
  proptest fuzz with no-panic guarantee (N7).
- **Session**: `NetStatus` transition-table tests; in-process two-
  `MinimalPlugins`-App loopback connect/teardown test (N2) — the same
  fixture style the repo already uses for headless app tests.
- **Lockstep**: fake-`NetOut` seam tests (delay math, empty ticks, stall,
  late-input drop, desync detection, SimPaused freeze) + the
  same-batch-stream proptest (N3).
- **End-to-end**: two in-process apps over real netcode UDP loopback,
  bot-vs-bot to a crowned winner with per-tick hash equality + non-vacuous
  fork injection (N6, in CI); `TETRIS_NET` desktop harness for real-network
  and two-machine sign-off (N6/N5).
- **Soak**: 20-match ignored-tagged nightly (N7), alongside the existing
  core soak.
- **Regressions**: all existing tests stay green unmodified (exact counts are
  a moving target — do not pin them); solo + local-versus gate-matrix tests
  (N4) guard the paths netplay bypasses. Rule bugs reproduce headlessly via
  the N3 seam first (repo discipline). Human gates follow the repo's
  author-sign-off rule.

## Risks & Mitigations

- **ReliableOrdered head-of-line stalls**: a lost batch packet stalls the
  guest mirror until retransmit; past ~RTT it shows as a hitch. Accepted for
  v1 (~2-4 KB/s at 60 Hz); mitigation path (Unreliable + NACK +
  re-request) explicitly deferred.
- **Netcode auth is Unsecure (v1)**: anyone reaching the port with the right
  `PROTOCOL_ID` can complete the handshake; the decode fuzz (N7) guarantees
  hostile bytes can't crash the host; the host additionally ignores all
  gameplay messages before handshake, and the Host screen warns the match is
  open while listening. Session tokens/invites deferred.
- **Second-guest silent drop** (netcode `max_clients: 1`): no `MatchFull`
  signal exists; surfaced as the `JoinTimeout` message wording ("host
  offline or match full").
- **Renet 2.0 API surface unverified at plan time**: `Cargo.lock` has no
  renet entries yet; N1's spike must pin *every* API this plan's N2/N3 code
  against and record it in `net/mod.rs` doc notes before wave 2 starts —
  not just the codec.
- **Input-delay misalignment**: `Hello.delay` + `MatchStart.match_delay`
  negotiate `D = max(peers)` so a host/guest `TETRIS_NET_DELAY` mismatch
  silently widens the delay instead of dropping inputs.
- **Lone-human preset override hijack**: `lone_human_p1` must treat `Net` as
  an occupied seat (N4.5) or the host loses WASD + solo-alternate keys;
  pinned by dedicated preset tests.
- **Rematch local reseed**: `start_versus` reseeds locally (verified
  `core_bridge/versus.rs:143-168`) — guest Rematch must be hidden and host
  Rematch routed via `MatchStart`; otherwise mirrors fork silently.
- **Overlay/HUD visibility regressions**: the repo has recorded bugs for
  active-versus click swallowing and overlay ZIndex; N3's teardown contract
  + N5's clickable-overlay test are mandatory, not optional polish.
- **Pause semantics**: pause chord off in `InMatch`; `SimPaused` is the only
  net-mode freeze (Lost/Desync); mid-match exit is Esc-confirm → `Bye`. All
  three stated, gated in named systems.
- **Headless CI limits**: full-binary CI E2E impossible without xvfb + audio
  workarounds — out of v1 scope; coverage comes from in-process real-transport
  tests + manual desktop harness.
- **Port collisions in tests**: fixed test ports (env override
  `TETRIS_TEST_NET_PORT`) with serial execution against the N2 socket test;
  documented.
- **`Controller` enum exhaustiveness**: the only external breakage site is
  `winner_text`; handled by N4's carve-out arm and clippy `-D warnings`.
