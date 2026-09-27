# PRD — Tetris-like Falling-Block Puzzle Game (Rust / Bevy)

- **Status:** M2-vertical-slice
- **Date:** 2026-09-27
- **Owner:** Project author
- **Working title:** `tetris` (placeholder — rename before release)

---

## 1. Summary

A fast, responsive, single-player falling-block puzzle game in the spirit of Tetris, built in
Rust on the Bevy ECS game engine. The game targets desktop first (Linux/Windows/macOS), with a
Web (WASM) build as a stretch goal. Gameplay follows modern community-standard mechanics:
SRS rotation, 7-bag randomizer, hold, ghost piece, lock delay, and soft/hard drop.

## 2. Problem / Opportunity

Bevy has no polished, idiomatic reference game of this genre. Building one produces:

1. A genuinely fun, replayable game with tight input feel.
2. A clean demonstration of ECS-driven game architecture: deterministic pure logic core
   decoupled from rendering/input/audio presentation layers.
3. A testable core (board/piece logic is pure Rust, unit-testable without the engine).

## 3. Goals

- **G1:** Arcade marathon mode with levels, escalating gravity, and standard scoring.
- **G2:** Modern controls feel: DAS/ARR handling, hold, ghost piece, hard drop, lock delay.
- **G3:** SRS rotation with wall kicks, including basic T-spin detection.
- **G4:** 60 FPS on integrated graphics; input latency imperceptible (< 1 frame).
- **G5:** Clean separation of simulation and presentation, enabling headless unit/property tests.
- **G6:** Configurable key bindings and volume from a settings screen.

## 4. Non-Goals (v1)

- Multiplayer (local or online).
- Guideline "Adventure"/mission modes, 40-lines sprint, ultra mode timers.
- Mobile/touch controls.
- User accounts, leaderboards, telemetry, monetization.
- Reskinning/piece themes beyond one default visual style.

## 5. Target Platforms

| Priority | Platform | Notes |
| --- | --- | --- |
| P0 | Desktop Linux / Windows / macOS | Native builds via `cargo`, CI later |
| P1 | Web (WASM via `bevy/web`) | Stretch; needs perf pass on UI text |

## 6. Gameplay Specification

### 6.1 Playfield

- 10 columns × 20 visible rows; 2 hidden buffer rows above for spawn.
- Line clear: full rows removed, stack shifts down. Multi-line clears score more (1–4 lines).
- Game over: a piece spawns overlapping existing cells (block-out) — top-out rule only.

### 6.2 Pieces (Tetrominoes)

All 7 standard pieces (I, J, L, O, S, T, Z), rendered as colored cells on a grid.

### 6.3 Randomizer

- **7-bag:** shuffle all 7 pieces, deal one at a time, reshuffle on empty.
- Preview queue: next **5** pieces shown (configurable 1–6).

### 6.4 Rotation — SRS

- Super Rotation System with JLSTZ and I kick tables.
- Clockwise, counter-clockwise (Q/E or mouse wheel), and 180° flip (A, optional binding).
- **T-spin detection:** 3-corner rule with kick-history exception (TST/TSD/TST-lite accepted at
  author's discretion). T-spins score bonus and count for back-to-back.

### 6.5 Movement & Drop

- Left/right shift with **DAS 150 ms / ARR 33 ms** defaults (configurable).
- Soft drop: +1 point per cell, gravity-speed × N.
- Hard drop: instant lock, +2 points per cell, triggers lock immediately.
- **Lock delay:** 500 ms grounded, reset on successful move/rotate, max **15 resets**.
- **Hold:** one swap per piece, first press spawns next piece from bag instead of swapping in.

### 6.6 Gravity / Levels

Level advances every **10 lines cleared**. Gravity table follows the Tetris Worlds curve,
capped at 20G; UI displays current level, lines, score.

### 6.7 Scoring (Guideline-based)

| Action | Base points (× level) |
| --- | --- |
| Single / Double / Triple / Tetris | 100 / 300 / 500 / 800 |
| T-spin mini / T-spin (no lines) | 100 / 400 |
| T-spin single / double / triple | 800 / 1200 / 1600 |
| Back-to-back Tetris/T-spin | ×1.5 |
| Combo | 50 × combo count × level (capped 10) |
| Soft drop / hard drop | 1 / 2 per cell (flat) |
| Perfect clear (single piece) | 3500 |

## 7. UX / Screens

1. **Title** — start, settings, quit.
2. **Game** — playfield, next queue (5), hold box, score/level/lines HUD, pause key hint.
3. **Pause** — resume, restart, settings, quit to title.
4. **Game over** — final score/level/lines, "best" persisted locally (JSON in config dir),
   restart, title.
5. **Settings** — key rebinding, master/SFX/music volume, DAS/ARR sliders, next-queue size.

## 8. Audio & Juice (M4 scope)

- SFX: move, rotate, lock, line clear (distinct for tetris/t-spin), level up, hard drop, hold,
  game over.
- Simple looping BGM track with pause ducking; Mute/sound in settings.
- Juice: line-clear flash + brief freeze (≤80 ms), hard-drop shake (subtle), ghost piece,
  lock-flash. All juice skippable in settings ("effects: low/med/high").

## 9. Controls (default bindings, all rebindable)

| Action | Default |
| --- | --- |
| Move left / right | ← / → |
| Soft drop | ↓ |
| Hard drop | Space |
| Rotate CW / CCW | ↑ / Z (X too) |
| Rotate 180 | A |
| Hold | C (Shift too) |
| Pause | Esc / P |

## 10. Technical Architecture

### 10.1 Stack

- **Rust** (stable, pinned via `rust-toolchain.toml` at scaffold), **edition 2021**.
- **Bevy** pinned to latest stable minor at scaffold time (0.19.x as of the 2026-09-26
  author amendment; bump deliberately).
- Plugins: `bevy_ecs` (in engine), `bevy_kira_audio` for audio, `serde` + `bevy_mod_rpchandling`-free
  simple JSON persistence (no extra state crate unless proven necessary).
- `cargo clippy -D warnings`, `cargo fmt`, `cargo test` gate in CI.

### 10.2 Crate layout

```
tetris/
├── Cargo.toml            # workspace root
├── crates/
│   ├── tetris-core/      # pure simulation: board, pieces, SRS, scoring, state machine. No Bevy.
│   └── tetris-app/       # Bevy binary: ECS wiring, scenes, rendering, input, audio, UI
```

`tetris-core` owns all rules and is deterministically testable (fixed RNG seed → replayable).
`tetris-app` subscribes to core events and animates them.

### 10.3 ECS design (app side)

- **Resources:** `GameCore` (wrapper over `tetris-core::Game`), `GameEvents` queue, `Settings`,
  `AudioHandles`.
- **Systems** (fixed-step `Simulation` set at 60 Hz, ahead of render):
  `input → apply actions → core step (gravity/lock) → produce events → visuals/audio response`.
- **Events:** `PieceSpawned`, `PieceLocked`, `LineCleared {rows}`, `ScoreChanged`, `LevelUp`,
  `GameOver` consumed by presentation systems.
- Rendering: 2D sprite batches / `bevy_ui` grid; cell size derived from window; letterboxed.
- Determinism core → trivial headless tests and seeded replays; replay export is post-v1.

### 10.4 Persistence

`~/.config/<app>/settings.json` + `best.json` via `serde`; corruption → reset to defaults, log.

## 11. Milestones & Plan

| # | Deliverable | Scope | Acceptance |
| --- | --- | --- | --- |
| **M0** | Repo scaffold (this PRD, README, .gitignore) | git, docs | CI-ready tree ✔ |
| **M1** | `tetris-core` | board, 7-bag, SRS + kicks, scoring, lock delay, hold, gravity | Unit tests: SRS kick tables, bag, scoring, line clears, T-spin detect; `cargo test` green |
| **M2** | Playable vertical slice | window, grid render, keyboard input (DAS/ARR), core wired, HUD, game over→restart | A human can play a full game; 60 FPS |
| **M3** | Full UX | pause, settings (rebind, DAS/ARR, volumes), best score, next×5, hold UI | All §7 screens reachable; settings persist |
| **M4** | Juice & audio | SFX, BGM, line flash, freeze frames, shake | "Feels good" pass; effects quality setting |
| **M5** | Release hardening | CI (fmt/clippy/test), release builds for 3 OSes, icon, README/screenshots | Tagged v0.1.0, downloadable binaries |
| **M6** | WASM build (stretch) | `bevy/web` config, reduced UI text, asset compression | Playable in browser |

Suggested ordering is strict: no milestone starts its polish before its acceptance gate passes.

## 12. Success Criteria

- Author plays ≥ 3 marathon sessions per week during development without friction complaints.
- Zero crash-to-desktop over 1 h automated headless soak (random action fuzz vs core).
- Clippy clean, core test coverage on all rules (SRS/scoring/lock/bag).
- New machine: `cargo run` works with no manual setup.

## 13. Risks & Mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Bevy API churn across minors | Rework | Pin version; upgrade only at milestones; core is Bevy-free |
| Input feel mistakes found late | Rework core API | Input schema decided in M1; core takes discrete actions with timestamps |
| WASM perf with text/UI | Stretch slips | M6 explicitly last, non-blocking |
| Scope creep (multiplayer) | Delay | §4 non-goals enforced |

## 14. Open Questions

1. Final game name & art style (flat color vs. sprite-based)?
2. Include 180 rotation and hold UI animation in M2 or defer to M3?
3. Sprint/40L mode as v1.1 quick win after M5?

## 15. Future Extensions (post-v1)

Local two-player versus, online multiplayer (bevy networking), replays/watch, additional modes
(sprint, ultra), skins, controller support, Steam packaging.
