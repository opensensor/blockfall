# Changelog

All notable changes to Blockfall are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/); versions follow SemVer.

## [Unreleased]

### Added

- **Android port**: `crates/tetris-app` now builds as an `android_arm64` /
  `android_x86_64` `cdylib` (`libblockfall_app.so`, `#[bevy_main]` entry)
  packaged by `scripts/build-android.sh` (javac/d8 + cargo-ndk +
  aapt2/zipalign/apksigner, no Gradle) against `android/AndroidManifest.xml`
  — debug-signed APK in `target/android/`. The activity is portrait-locked
  and immersive-fullscreen: `dev.blockfall.app.Main` (`android/java/`, a
  `NativeActivity` subclass shipped as `classes.dex`) hides the nav pill and
  status bar sticky-style on create/resume/focus, `targetSdk 34` because
  Android 15+ ignores system-bar hiding for apps targeting 35+.
  **Portrait-native layout**: `render::playfield_view` reserves a HUD strip
  above the field (`LEVEL | SCORE | LINES`, hold box + horizontal next
  queue) and a touch deck below (`combo`/`B2B` row under the field);
  landscape windows keep the classic side-panel HUD, versus panels and the
  full button cluster. New `crates/tetris-app/src/touch.rs`: playfield
  **gestures** (tap = rotate, horizontal drag = shift per cell, swipe down =
  soft drop, flick down = hard drop) plus a compact disc row (`CCW CW HOLD
  DROP`) and pause disc on portrait; a landscape deck (`< > v` / `CCW CW
  DROP` / `HOLD` / `II`) auto-swaps in for rotated/desktop windows —
  desktop smoke-testable with `TETRIS_TOUCH=1` (`TETRIS_PORTRAIT=1` forces
  the portrait layout). All input drives the existing DAS/ARR/soft-drop
  repeat machines and the local versus seat. Android details: settings/best
  scores write to the app's private internal storage (no `dirs` home),
  lifecycle suspend auto-pauses a live match, the Online → Join entry
  auto-focuses the Android soft keyboard, and the desktop "Quit" buttons
  are compiled out (back gesture exits). Verified on an API 36 emulator and
  a Pixel 10 Pro XL.
- **Netplay gateway & room codes** (gateway-plan.md G1–G5): cross-WAN play
  with zero setup on both sides.
  - **Gateway** (`crates/netplay-gateway`, new zero-dependency crate):
    introduce + dumb port-paired relay — one UDP control port, one virtual
    data port per room, netcode packets stay opaque ciphertext; pure
    virtual-time room state machine (register/busy/collision/GC/rate limit)
    with unit + loopback integration tests and a `--self-test` flag that
    verifies any build end-to-end in one command.
  - **Room codes**: hosting shows `Room ABCDE — share with a friend`
    (5 chars, `I L O 0 1` excluded from the alphabet); the Join screen opens
    in **Code** mode (toggle to IP), with per-step status — `resolving…` /
    `joining room…` / `no such room` / **`match full`** / `gateway full —
    retry later` / `gateway offline — check connection or join by IP`. Busy
    and not-found answer in milliseconds instead of the old silent 10 s
    JoinTimeout. Guests need only outbound UDP; hosts need no port forward at
    all (UPnP becomes the no-gateway fallback).
  - **Config**: `TETRIS_GATEWAY=<host:port>` selects the relay (default
    `netplay.opensensor.xyz:27016`; empty disables the feature entirely),
    plus a persisted `gateway_enabled` toggle in `net_profile.json`. A down
    gateway never blocks a match — one Host-screen line, and the direct-IP /
    UPnP paths continue untouched.
  - **Packaging / ops**: CI builds a static musl gateway binary attached to
    releases; scratch Dockerfile (UDP 27016–27999), hardened systemd unit,
    `scripts/gateway-smoke.sh`, and an ops README in the crate.
  - **E2E**: a full bot-vs-bot Garbage match relayed end-to-end through the
    real in-process gateway over loopback UDP with per-60-tick snapshot-hash
    stream equality and `*D` room-release verification at teardown, plus
    gateway-down and busy-room tests — all riding normal
    `cargo test --workspace`. The crown test also caught and fixed a client
    edge: the room is no longer released on the peer-connect transition,
    which would have torn the relay out from under the first match.
- **Hosted netplay gateway** (`deploy/`): the production gateway now runs at
  **`blockfall.opensensor.io:27016`** (data ports 27017–27216) on the
  opensensor home server — `DEFAULT_GATEWAY_ENDPOINT` switched from the
  unregistered `netplay.opensensor.xyz` to it. Ops stack: hardened systemd
  unit + `blockfall-update` pull-agent (picks up the CI-built musl release
  automatically, self-tests before swap) and a `blockfall-dns` agent that
  keeps the DynamicDNS-style A record pointed at the home egress IP (failover
  to a cloud standby slots in later). See `deploy/README.md`.
- **UPnP IGD auto port mapping for cross-WAN hosting** (netplay-plan.md
  addendum): hosting now asks the home router to forward the UDP port via
  SSDP discovery + SOAP `AddPortMapping` (no new dependencies — hand-rolled
  std client) and shows `Friends join at <public-ip>:<port>` on the Host
  screen; the 1 h lease self-renews every 30 min while the session holds and
  is deleted on `net_stop`/app exit. Routers without UPnP, blocked SSDP or
  ISP-controlled edge devices show a one-line manual-forward hint instead —
  LAN play is never affected. `U` on the Host screen toggles the attempt
  (persisted as `upnp_enabled` in `net_profile.json`, additive serde
  default), and hosted UDP tunnels (ngrok/cloudflared) are documented as
  dead ends for raw UDP in 2026.

## [0.2.0] — 2026-09-29

### Added

- **Netplay v0.1 — 1v1 versus, local and online** (tetris-plan.md T24–T26 +
  netplay-plan.md N1–N8)
  - **Versus ruleset (`tetris-core`)**: `Match` over two independent games with
    `Garbage` attacks (queued lines, chain bonus, one-hole garbage batches) and
    `Race` (default 40 lines); deterministic from the match seed; top-out crowns
    the opponent
  - **Local versus**: shared-keyboard play with fixed P1 (WASD) / P2 (arrows)
    presets and human-or-bot seats, side-by-side dual viewports, per-side versus
    HUD with pending-garbage counters, Title → 1 v 1 menu flow (Garbage/Race),
    winner overlay with rematch
  - **Online 1v1**: direct IP/port join over `bevy_renet` + netcode (no lobby,
    relay or LAN discovery — post-v1); host listens on UDP 27015 and shows a
    connect hint, guest types `ip:port` (keyboard-only entry); host = left board
    (P1 preset), guest = right (P2 preset)
  - **Lockstep sync**: deterministic mirror from a shared seed; inputs delayed D
    ticks (D = max of both peers' `TETRIS_NET_DELAY`, default 8 ≈ 133 ms,
    range 2–30); no rollback — the opponent's board, HUD and pending garbage
    come from the local deterministic mirror
  - **Robustness**: protocol-version handshake rejects mismatched builds;
    periodic snapshot-hash checks detect desync and freeze both boards behind an
    explicit overlay with clean teardown; last join address persists to
    `net_profile.json`
  - **Net-match rules**: no pause (Esc = leave-with-confirm, graceful bye to the
    peer), host-only rematch; v1 netcode auth is Unsecure — anyone reaching the
    port with the right protocol ID can join while listening
  - **Harness & CI**: `TETRIS_NET=host:<port>` / `join:<ip:port>` desktop
    bot-vs-bot-across-the-wire mode (hash-logged, exit-code checked),
    `TETRIS_NET_FORK=guest:<tick>` / `host:<tick>` desync-injection hook, and
    in-process two-app end-to-end tests over real netcode UDP loopback
    (per-60-tick hash-stream equality + non-vacuous fork detection) in
    `cargo test --workspace`

## [0.1.0] — 2026-09-27

First public release.

### Added

- **Ruleset (`tetris-core`, headless, 230+ tests incl. proptests + 1 h soak)**
  - 10×22 playfield with 2-row spawn buffer; full SRS rotation system with
    wall kicks and kick-bound clamping (`MIN_ROT_ROW`)
  - 7-bag randomizer (seedable via `TETRIS_SEED`), hold (one per drop),
    hard/soft drop, lock delay with move-reset (grounded-only, 15 steps),
    line clears with scoring, combos and back-to-back bonus
  - Deterministic game facade (`Game::new(seed)`, `snapshot`) — pure logic,
    zero rendering dependencies
- **Game (`blockfall`, Bevy 0.19)**
  - Title / Playing / Paused / Settings / Game Over screens; pause chord
    (Esc/P) freezes the simulation, not the UI
  - Settings: volume, SFX toggle, key rebinding with live capture; settings
    and best score persist under `~/.config/tetris/`
  - Input: DAS/ARR auto-repeat (9/2 ticks at 60 Hz), remappable, mouse
    support for menus
  - Ghost piece, hold + next-queue previews, score/level/lines HUD
  - Audio: synthesized SFX set (line clear, lock, hold, spawn, drop,
    game-over) with volume/toggle wiring
  - Game feel: line-clear flash, screen shake, hit-stop freeze on clears
  - Release binaries for Linux, Windows, macOS attached to GitHub releases
- **Engineering**
  - GitHub Actions: fmt + clippy (`-D warnings`) + test matrix on every
    push/PR; nightly 1 h wall-clock soak of the core ruleset
  - `TETRIS_BOT` deterministic solver marathon harness (60 fps proof),
    `TETRIS_SHOT` screenshot hook for visual regression checks
  - GPLv3 license

[0.2.0]: https://github.com/opensensor/blockfall/releases/tag/v0.2.0
[0.1.0]: https://github.com/opensensor/blockfall/releases/tag/v0.1.0
