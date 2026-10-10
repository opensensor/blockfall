# Changelog

All notable changes to Blockfall are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/); versions follow SemVer.

## [Unreleased]

### Added

- **Horror mutator** — night render: the active piece is a downward-casting
  flashlight. A halo hugs the piece and a shaft of light falls from it,
  widening with depth, dimmed off the beam edges and losing throw toward
  the well floor (vintage lamp falloff); the beam sweeps as the piece moves
  side to side. The well grid is muted under night-shade quads and only
  reveals itself where the beam (or a line-clear flash) lights it. The
  ghost dims by its own beam position and the active piece stays fully
  lit. Render-only — snapshot, replay and records untouched — and composes
  with Invisible.

## [0.6.0] — 2026-10-04

### Added

- **Soundtrack selection** — two new synthesized BGM loops join the classic
  track (`bgm_pulse.wav`: driving 128 BPM arpeggio; `bgm_drift.wav`: slow
  ambient pad), selectable in Settings under Music. Switching restarts the
  loop live and persists across runs; `settings.json` files from earlier
  builds keep all their settings and adopt Classic.
- **Mode-select redesign** — rows are now rounded accent cards with a
  colored mode glyph, dimmed description and right-pinned gold record;
  hover highlights the card, Up/Down move a visible cursor, and Enter
  starts the selected row (Bot Ladder keeps its ladder-screen routing).
  The Daily banner is a gold card and the mutator toggles have a caption.
- **Auto soundtracks** — a fourth choice picks per mode (Zen drifts, Ultra
  pulses, everything else classic) and re-resolves live when the mode
  changes; the cycle is Classic → Auto → Pulse → Drift.
- **Combo pitch riser** — lock/clear/move SFX climb a semitone-ish step per
  combo level (capped at +20%), giving audible feedback for streaks.
- **Ultra final-10-s heartbeat** — an amber screen pulse once per remaining
  second mirrors the audio warning, ending together with the clock.
- **Per-run stats line on game over** — pieces-per-second, lines/min,
  t-spins, tetrises and max combo, derived from the event stream.
- **Daily share image** — game over after a daily run shows a "Save share
  image" button (or press `S`) that writes `blockfall_daily_<date>.png`
  next to the game; the share line itself is now ASCII so the headline
  renders in-game.
- **Colorblind palette** — an Okabe–Ito-inspired piece set selectable in
  Settings (Colors row); it applies to playfield, previews, hold, queue and
  versus HUD.
- **Reduce flash** — settings toggle that suppresses screen flashes (event
  flashes and the Ultra heartbeat) while keeping shake and freeze frames.
- **Menu card restyle** — title/pause/game-over buttons are now rounded
  cards with a visible border that lights up on hover and press.

### Fixed

- **Ultra ended after 2 minutes** — the advertised six-minute clock was
  encoded as 7 200 ticks, which is 120 s at 60 Hz; the budget is now
  21 600 ticks (6 min), matching the mode card and README.
- **Audio was silent in shipped builds** — `AudioPlugin` defaulted its
  `AudioEnabled` guard to `false` and no production path ever set it `true`,
  so BGM/SFX never played (the documented production default). The guard now
  defaults to enabled whenever the Kira stack is wired; headless tests stay
  guarded.
- **Daily banner showed tofu** — its `·`/`—` separators are missing from
  the bundled font subset and rendered as boxes; the banner and the daily
  share line (now shown as the game-over headline) are ASCII throughout.

## [0.5.0] — 2026-10-03

### Added

- **Modernized default look (phase 1)** — all game art is now generated in
  code at startup (`art.rs`, zero asset files): beveled top-lit tiles with
  rounded corners (blocks read as blocks — adjacent same-color cells no
  longer merge into blobs), an outlined ghost ring instead of a dimmed
  silhouette, a dark playfield well panel with a faint cell grid, and a
  radial edge vignette over a new deep-navy clear color. The HUD next,
  hold and versus previews reuse the same beveled tile. Headless test apps
  without the asset pass keep the old flat rendering.
- **Event-colored screen flashes** — line clears flash cyan (scaling to
  bright ice on tetrises), T-spins violet, perfect clears gold, level ups
  mint and game overs red instead of raw white; the flash is clamped to the
  playfield well (solo) or the shared versus view instead of the whole
  window, so pillar-boxed voids and HUD strips stay clean.

### Fixed

- The Daily Challenge banner test pinned expectations to a hardcoded date
  whose override never actually reached the banner (thread-local date
  override vs. scheduler worker threads) — it silently depended on the
  real calendar date and now derives from `daily::today()`.

## [0.4.0] — 2026-10-01

### Added

- **Game modes** — seven playable single-player modes alongside Marathon:
  Sprint (40 lines), Ultra (6-minute score), Dig (10 buried garbage rows),
  Survival (rising garbage feed), Zen (no game over — a top-out wipes the
  stack), Bot Ladder (eight bots of rising speed, unlock persisted) and the
  Daily Challenge (one seeded run per UTC day; the whole world gets the same
  board and mode, shareable as a text line). Mode-select screen, per-mode
  HUD (clocks, goals, garbage meters) and mode-aware result screens with
  per-mode best records.
- **New versus attack rules** — Dig Duel (both players race the same seeded
  buried garbage board with the same piece sequence; first to clear wins,
  first to top out loses) and Switch (Garbage attacks plus a full
  board-swap of both players' states every 30 s with a 3 s warning). Both
  selectable locally and online.
- **Mutators** — per-run toggles on the mode-select screen: No Hold, No
  Ghost, One Preview, 20G and Invisible (locked cells fade out after 1 s).
  Mutated runs still count plays but never write best records.

### Changed

- **Net protocol `0.1.0` → `0.2.0`** — `AttackRule` gained the Dig and
  Switch variants and match snapshots carry a match-level tick clock. The
  version handshake refuses mixed builds: **desktop and Android must be
  updated together**.
- Records file migrated to per-mode keys (one-time, automatic); the nightly
  netplay soak now covers all four attack rules (20 matches each).

## [0.3.2] — 2026-09-30

### Fixed

- **Guest controls dead in relayed (room-code) matches (field fix)**: the
  first real two-machine room-code match connected and started, but only
  the host could play — every guest `TickInput` reached the host after its
  target tick, and the deterministic empty-input fallback silently ran in
  its place. Two compounding causes, two fixes: (1) the gateway's relay
  poll loop ran every 100 ms, adding up to one full period per hop — now
  10 ms (draining a handful of nonblocking sockets on a 10 ms wake is
  negligible); (2) the negotiated input delay (default 8 ticks ≈ 133 ms)
  was a fixed constant, too small for any path slower than LAN — the host
  now raises it at match start from the measured netcode round trip
  (one RTT + 4 ticks of jitter, clamped 2..=30; the RTT is measured
  game-socket-to-game-socket, so relay hops count), and the guest adopts
  it through the existing `MatchStart.match_delay` channel. Late drops are
  no longer invisible: the first five warn on the host log, then
  rate-limit, and the gateway-relay E2E now asserts both directions — the
  guest side must apply actions at the host, and nothing may be late — so
  an inert-guest relay match can never pass again (before this, hash-
  stream equality alone let it: 258/259 guest inputs late, test green).
  The E2E's relay match also moved to 4x virtual speed so the real-time
  relay latency no longer eclipses the delay budget outright.

## [0.3.1] — 2026-09-30

### Fixed

- **Room-code hosting across consumer NAT (field fix, gateway-plan.md
  "Field fix: host punch")**: room codes paired reliably but guests then
  sat in `Connecting` forever against a real host NAT — the host's game
  socket never sent a packet, so the host router had no inbound mapping for
  the relay's forwarded connect requests. The relay now announces the room's
  virtual data port in a new `*V` frame after every `*A`; the host punches
  that address once from the held game socket before netcode takes it over,
  and the relay retargets the room's host-side address on the punch (and on
  every host-source packet, following NAT port rotation) instead of the
  advertised `game_port`. Hosting with the gateway armed is now a two-phase
  bind (`Listening` is immediate; the netcode transport comes up when the
  announce window closes, ≤ 3 s), connect requests that arrive mid-window
  are served from the socket's receive buffer, and the wire stays
  backward/forward compatible in both directions (old games drop the unknown
  frame; new games hand the socket over punch-less when no `*V` arrives).
  A `*F`-then-dead-air guest now reads
  `host unreachable — ask host to enable UPnP or port-forward UDP 27015`
  instead of the generic loss line.
- **Test isolation**: the full-plugin smoke tests now pin `TETRIS_CONFIG_DIR`
  to a private temp dir under the settings tests' shared env lock, fixing an
  intermittent `das_ms 111 vs 150` race when a settings-persistence test
  wrote its temp dir mid-boot.

## [0.3.0] — 2026-09-30

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
  a Pixel 10 Pro XL. Release tags now build the universal APK in CI
  (Release workflow `android` job — SDK pinned to build-tools 37.0.0 /
  platform 36 / NDK r30) and attach it to the GitHub release as
  `blockfall-<tag>-android.apk`.
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

[0.3.1]: https://github.com/opensensor/blockfall/releases/tag/v0.3.1
[0.3.0]: https://github.com/opensensor/blockfall/releases/tag/v0.3.0
[0.2.0]: https://github.com/opensensor/blockfall/releases/tag/v0.2.0
[0.1.0]: https://github.com/opensensor/blockfall/releases/tag/v0.1.0
