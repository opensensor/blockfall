# Changelog

All notable changes to Blockfall are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/); versions follow SemVer.

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
