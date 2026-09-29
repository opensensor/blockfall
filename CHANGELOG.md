# Changelog

All notable changes to Blockfall are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/); versions follow SemVer.

## [Unreleased]

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

[0.1.0]: https://github.com/opensensor/blockfall/releases/tag/v0.1.0
