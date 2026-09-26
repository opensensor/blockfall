# Plan: Tetris-like Game — Rust/Bevy Implementation (M1–M6)

**Generated**: 2026-09-25
**Source**: [PRD.md](PRD.md) (v0.1)

## Overview

Build the game defined in the PRD: a pure, deterministic rules crate (`tetris-core`) wrapped by
a Bevy presentation binary (`tetris-app`). Tasks are atomic, single-file-ownership-scoped, and
dependency-explicit so independent agents can run in parallel. Strict milestone gating applies:
M1 tasks gate all app work; M2 slice gates M3/M4 polish.

**Resolved decisions (from author, 2026-09-25):**
- Flat colored cells — no art asset pipeline; sprites/themes are post-v1.
- M2 vertical slice **includes** 180° rotation and hold (both are core rules anyway).
- CI is **local-only until M5** — no GitHub Actions until release hardening milestone.

## Prerequisites

- Rust stable 1.90 (`rustup`), `git`, Linux dev box (cross-builds for Win/macOS only in M5).
- Verified dependency versions (crates.io, 2026-09-26 amendment — **author directive: use
  latest stable Bevy, supersedes the 0.18.1 pin**):
  - `bevy = 0.19.1` (max stable; 0.20.0-rc.1 exists — do **not** use). Feature collections:
    use `default-features = false, features = ["2d", "ui"]` (`2d` already pulls `ui`, `audio`,
    `scene`, `picking`).
  - `bevy_kira_audio = 0.26.0` — declares `bevy ^0.19.0`, so compatibility with 0.19.1 is
    confirmed from its manifest (T18's compile spike is now a formality; fallback remains
    Bevy's built-in `audio` feature, already enabled by `2d`).
  - `serde = 1.0.229` + `serde_json`, `dirs = 7.0.0`, `proptest = 1.11.0` (dev-dep).
- Bevy 0.19 references for app tasks: standard UI widgets emit `ValueChange<T>` events;
  `AutoDirectionalNavigation` for keyboard/gamepad menu focus; `EasyScreenshotPlugin` for
  M5 README screenshots; fixed-timestep update pattern per Bevy 0.19 `FixedTimestep` docs.
  API names verified at T1 scaffold; app tasks re-verify against `cargo doc` before use.
- Parallel-agent rules:
  - **T1 pre-creates the full module tree and ALL workspace dependencies** so parallel tasks
    never co-edit `Cargo.toml` or `lib.rs`/`mod.rs` wiring. After T1: **no dependency
    additions**; manifest *metadata* edits are allowed only in T21 (bin/name) and T23 (wasm
    feature + `getrandom/js`).
  - T1 defines one Bevy `Plugin` stub per app module (`RenderPlugin`, `InputPlugin`,
    `HudPlugin`, `MenuScreensPlugin`, `SettingsScreenPlugin`, `AudioPlugin`, `JuicePlugin`)
    and **registers all of them in `main.rs` at T1**. Later app tasks only fill their own
    `Plugin::build()` — `main.rs` is never re-edited.
  - Single-owner data types: `Settings`, `AppState`, `RebindingCapture`, `EffectsQuality`
    structs/enums live in `state.rs` (created T1); other tasks only read or persist them.
  - The core→app contract (`Game::snapshot()` + `GameEvent` variants) is fixed by T8 and
    treated as frozen; app tasks T11/T13/T18/T19 code against it verbatim.

## Dependency Graph

```
T0 (PRD/scaffold, done)
 └─ T1 ─┬─ T2 ──┬─ T4 ──┬─ T6 ─┐
        │       ├─────── T7 ────┼─ T8 ─┬─ T9
        ├─ T3 ──────────────────┘      │
        └─ T5 ─────────────────────────┤
                                       ├─ T10 ─┬─ T11 ── T13 ─┐
T9 (tests, parallel)                   │       ├─ T12 ────────┼─ T14 ─┬─ T17 ─┐
                                       │       ├─ T15 ─┬─ T16 │       ├─ T21  ├─ T20 ─┐
                                       │       └─ T18 ─┴─ T19 ┘       │       │       ├─ T22 ── T23
                                       └──────────────(events)────────┴───────┴───────┘
```

Textual form:

```
T1:  []            workspace scaffold (module stubs + all deps)
T2:  [T1]         core types & board
T3:  [T1]         7-bag randomizer
T5:  [T1]         gravity/level curve
T4:  [T2]         SRS rotation + kicks + 180
T6:  [T2,T3,T4]   actions, hold, lock delay
T7:  [T2,T4]      scoring, combo/B2B, T-spin detection
T8:  [T4,T5,T6,T7] Game facade (headless deterministic loop)
T9:  [T8]         proptest invariants + fuzz soak
T10: [T1,T8]     app bootstrap (window, fixed-step, GameCore, events)
T11: [T10]       renderer (grid, ghost, letterbox)
T12: [T10]       input (bindings, DAS/ARR → Actions)
T15: [T10]       settings/best-score persistence
T18: [T10]       audio (SFX/BGM, ducking)
T13: [T11]       HUD (score/level/lines, next×5, hold box)
T14: [T10,T11,T12,T13] vertical slice integration + playtest  ← M2 gate
T16: [T12,T15]   settings screen + rebinding UI
T17: [T14,T15,T16] title/pause/game-over screens, best display  ← M3 gate
T19: [T11,T15,T18] juice (flash, freeze, shake) + effects setting
T21: [T14]       icon, screenshots, README/docs polish
T20: [T17,T19]   CI (GH Actions) + 3-OS release builds       ← M5 opens
T22: [T20,T21]   v0.1.0 release, tag
T23: [T22]       WASM build (stretch, M6)
```

## Tasks

### T0: PRD & repo bootstrap
- **depends_on**: []
- **location**: `PRD.md`, `README.md`, `.gitignore`
- **description**: Git init, PRD v0.1, README, gitignore. (Done — committed.)
- **validation**: `git log` shows root commits.
- **status**: Completed
- **log**: 2026-09-25 — commits b9ef318, b8588f2.
- **files edited/created**: PRD.md, README.md, .gitignore

### T1: Workspace scaffold
- **depends_on**: []
- **location**: `Cargo.toml`, `rust-toolchain.toml`, `crates/tetris-core/**`, `crates/tetris-app/**`, `rustfmt.toml`
- **description**: Cargo workspace with `tetris-core` (lib) and `tetris-app` (bin, Bevy 0.19.1,
  `default-features = false, features = ["2d","ui"]`). Pre-create the FULL module stub tree so
  parallel tasks never share files: core = `board.rs`, `piece.rs`, `bag.rs`, `srs.rs`,
  `gravity.rs`, `actions.rs`, `lock.rs`, `hold.rs`, `score.rs`, `tspin.rs`, `game.rs`,
  `event.rs`, `prng.rs`; app = `main.rs`, `state.rs`, `core_bridge.rs`, `render.rs`, `input.rs`,
  `hud.rs`, `screens_menu.rs`, `screens_settings.rs`, `settings_persist.rs`, `audio.rs`,
  `juice.rs` (all compiling stubs). In `state.rs` define the shared types with their final
  shapes: `Settings` (serde `Serialize/Deserialize`, PRD defaults: DAS 150 ms / ARR 33 ms /
  volumes / next-queue 5 / effects quality), `AppState` enum (Title/Playing/Paused/Settings/
  GameOver), `RebindingCapture` resource, `EffectsQuality` enum — single owner here, later
  tasks only consume. In `main.rs` register `Plugin` stubs for every app module
  (`RenderPlugin`, `InputPlugin`, `HudPlugin`, `MenuScreensPlugin`, `SettingsScreenPlugin`,
  `AudioPlugin`, `JuicePlugin`); later tasks fill only their own `build()`. Declare ALL
  workspace deps here (bevy, bevy_kira_audio, serde, serde_json, dirs, proptest dev-dep).
  Add `[profile.dev] opt-level = 1` for deps.
- **validation**: `cargo test` green incl. a headless smoke test (App with `visible: false`
  windows builds and exits after N frames); `cargo run` opens a window manually; `cargo fmt
  --check` and `cargo clippy --all-targets -- -D warnings` clean.
- **status**: Completed
- **log**: 2026-09-26 — commit 429b93e. Bevy bumped 0.18.1→0.19.1 per author directive;
  bevy_kira_audio 0.26.0 compat confirmed (builds against 0.19.1, no fallback needed).
  Toolchain: `rust-toolchain.toml` pinned 1.90→**1.95** — Bevy 0.19.1's MSRV is rustc 1.95.0
  (cargo refused to resolve under 1.90); prerequisites' "Rust stable 1.90" line is superseded
  by this. 0.19 API drift fixed in stubs: `init_resource` requires `FromWorld`/`Default`
  (`RebindingCapture` given a final shape with `capturing: bool` flag — a presence-only unit
  struct was incompatible with `main.rs`'s permanent `init_resource` wiring); plugin tuples no
  longer implement `PluginGroup` (plain `add_plugins((..))` still works); winit refuses
  event-loop creation off the main thread, so the hidden-window smoke test uses
  `MinimalPlugins + WindowPlugin` (no winit) — fully headless, no display needed, <0.1 s.
  `cargo build/test --workspace` green (2 headless smoke tests), `cargo fmt --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean. TDD `reason_not_testable`: scaffold task;
  the headless smoke test is itself the acceptance artifact. Deferred manual check: interactive
  `cargo run` window (no display exercised in this environment).
- **files edited/created**: Cargo.toml, Cargo.lock, rust-toolchain.toml, rustfmt.toml,
  crates/tetris-core/Cargo.toml, crates/tetris-core/src/{lib,actions,bag,board,event,game,
  gravity,hold,lock,piece,prng,score,srs,tspin}.rs, crates/tetris-app/Cargo.toml,
  crates/tetris-app/src/{main,state,core_bridge,render,input,hud,screens_menu,
  screens_settings,settings_persist,audio,juice}.rs

### T2: Core types & board
- **depends_on**: [T1]
- **location**: `crates/tetris-core/src/{piece.rs,board.rs}`
- **description**: `Piece` enum (I,J,L,O,S,T,Z), cell-grid `Board` (10×22, 2 hidden rows):
  spawn/collide/merge/line-scan/shift-down, `is_empty()`. Also the pure ghost helper
  `ghost_row(board, piece_state) -> row` (lowest valid row for the piece in its current
  rotation — keeps ghost logic out of the renderer). Pure, no RNG.
- **validation**: Unit tests — collision at walls/floor/stack, full-row detection, shift-down
  ordering, spawn rows, out-of-bounds guards, `ghost_row` on partial-stack fixtures.
  `cargo test -p tetris-core` green.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T3: 7-bag randomizer
- **depends_on**: [T1]
- **location**: `crates/tetris-core/src/{prng.rs,bag.rs}`
- **description**: Dependency-free seedable PRNG (splitmix64/xorshift). `Bag::new(seed)`,
  `next()`, `peek(n)`; every 7 consecutive deals are a permutation of all pieces; same seed →
  same sequence.
- **validation**: Unit tests — permutation invariant over 1000 bags, determinism, peek ≤ queue
  depth auto-refills.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T4: SRS rotation, wall kicks, 180°
- **depends_on**: [T2]
- **location**: `crates/tetris-core/src/srs.rs`
- **description**: Precomputed cell offsets per (piece, rotation state 0–3 + 180 states if
  modeled), JLSTZ and I kick tables per SRS spec, `try_rotate()` returning success + kick
  index (stored for T-spin). 180°: kick trials per common guideline (no-kick first, then
  standard set). Pure function of board + piece state.
- **validation**: Table-driven tests for every piece/state/direction incl. classic kicks
  (I against left wall, T spin setup, O no-op). Kick index reported correctly.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T5: Gravity & level curve
- **depends_on**: [T1]
- **location**: `crates/tetris-core/src/gravity.rs`
- **description**: Tetris Worlds seconds-per-row table for levels 1–19, ≥20G cap; convert to
  tick counts at the core's 60 Hz logical clock; `interval_for(level)` + `level_for(lines)`
  (level up every 10 lines).
- **validation**: Boundary tests: level 1 = 1.000 s/row = 60 ticks @ 60 Hz, level 19 = 1/19 s,
  level 20 = 1/20 s = 3 ticks (20G cap), lines 9/10/19/20 → levels 1/2/2/3.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T6: Actions, hold, lock delay
- **depends_on**: [T2, T3, T4]
- **location**: `crates/tetris-core/src/{actions.rs,lock.rs,hold.rs}`
- **description**: `Action` enum (Move/SoftDrop/HardDrop/RotateCw/RotateCcw/Rotate180/Hold) —
  the input schema PRD §13 requires locked early. Lock delay: 500 ms grounded at 60 Hz, reset
  on successful move/rotate, max 15 resets then force-lock; hard drop locks same tick. Hold:
  one-per-piece flag, first press swaps in from bag head, hold cell persists.
- **validation**: Unit tests — force-lock after 15 resets, timer resets on successful shift but
  not on failed move, double-hold rejected, first-hold consumes bag not previous hold slot.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T7: Scoring, B2B/combo, T-spin detection
- **depends_on**: [T2, T4]
- **location**: `crates/tetris-core/src/{score.rs,tspin.rs}`
- **description**: PRD §6.7 table incl. ×level multiplier, flat drop points, B2B ×1.5 for
  Tetris/T-spin chains, combo 50×n×level capped at 10, perfect-clear 3500 (board empty after
  lock). T-spin: 3-corner rule + last-kick exception; classify full/mini/none using kick
  index from T4.
- **validation**: Table tests per PRD row; B2B break on plain clear; combo cap; TSD vs TST vs
  mini fixtures; drop points unaffected by level.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T8: `Game` facade — headless deterministic loop
- **depends_on**: [T4, T5, T6, T7]
- **location**: `crates/tetris-core/src/{game.rs,event.rs}`
- **description**: `Game::new(seed)` + `tick()` / `apply(Action)` → `Vec<GameEvent>`
  (PieceSpawned/PieceLocked/LineCleared/ScoreChanged/LevelUp/TSpinDetected{kind}/
  HoldPerformed/PerfectClear/ComboChanged{n}/GameOver). Block-out game over at spawn;
  top-out only per PRD. **Frozen contract for app tasks:** also `Game::snapshot() ->
  GameSnapshot` with board cells, active piece state, `ghost_row`, hold contents +
  hold-used flag, next queue (1–6), score/level/lines/combo/b2b — enough to render a full
  frame from scratch (e.g. right after restart) with events alone not required. All rules
  delegate to T2–T7 modules.
- **validation**: Scripted integration tests — same seed + same action log → identical score &
  final board (replay determinism); marathon fixture: Tetris + TSD + combo sequence scores
  exactly expected value; block-out triggers GameOver; snapshot completeness test (fields
  cover everything T11/T13/T18 need; assert TSpinDetected/HoldPerformed/PerfectClear/
  ComboChanged emitted in a fixture run).
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T9: Property tests & fuzz soak
- **depends_on**: [T8]
- **location**: `crates/tetris-core/tests/{invariants.rs,soak.rs}`
- **description**: proptest: no overlapping cells ever, score monotonic, board invariants,
  hold/lock invariants under random action sequences. Wall-clock-bounded soak (PRD §12 "1 h
  crash-free"): `#[ignore]`-gated test running random actions for ≥1 h CPU time, executed by
  the dedicated nightly CI job added in T20 (never in per-push CI).
- **validation**: `cargo test -p tetris-core` green; `cargo test -p tetris-core -- --ignored`
  completes the timed soak without panic; shrinking yields minimal failing cases on injected
  bugs (spot-check).
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T10: App bootstrap & fixed-step core bridge
- **depends_on**: [T1, T8]
- **location**: `crates/tetris-app/src/{main.rs,state.rs,core_bridge.rs}`
- **description**: Window (1280×720, resizable), camera, 60 Hz fixed-timestep simulation
  schedule ahead of render. `GameCore` resource wrapping `tetris-core::Game` (seeded from
  time), `PendingActions` queue consumed by the fixed-step bridge, `SimPaused` gate on the
  bridge (used later by T19 freeze-frames; render keeps running), insertion of the T1-owned
  `Settings`/`AppState`/`RebindingCapture` resources, core events drained into Bevy
  `Events<GameEvent>`. Initial state: Playing; Playing↔GameOver transition minimal (full
  screens in T17).
- **validation**: Headless test: simulated ticks advance core; restart resets core AND
  snapshot is fully re-renderable from tick 0; no sim work in render schedule; `SimPaused`
  truly halts core stepping while events still drain.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T11: Playfield renderer
- **depends_on**: [T10]
- **location**: `crates/tetris-app/src/render.rs`
- **description**: Flat colored cells (author decision) drawn **exclusively from
  `Game::snapshot()`** — board, active piece, and ghost (ghost row comes from the core's
  `ghost_row`, the renderer computes no rules); cell size derived from window with
  letterboxed 10×20 view; PRD piece colors; full-redraw per frame acceptable at this scale.
- **validation**: Unit-test the letterbox rect math (cell size/offset at several window
  sizes); headless render-app smoke test; visual: stack/active/ghost correct under resize;
  ≥58 FPS avg measured via frame-time log in a 60 s scripted run (method stated in test).
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T12: Input map, DAS/ARR
- **depends_on**: [T10]
- **location**: `crates/tetris-app/src/input.rs`
- **description**: Rebindable `KeyBindings` resource (owned here): PRD §9 defaults **plus**
  wheel-up/down → RotateCw/Ccw and the X / Shift aliases; the PRD §6.4 "Q/E" note is
  superseded by §9 + wheel (resolve in favor of §9, record in code comment). A `Pause` chord
  lives in the same table but is consumed by the menu-state handler (T17), never emitted as a
  core `Action`. DAS/ARR state machine runs **on the fixed-step schedule in tick counts**
  (defaults: DAS 150 ms → 9 ticks, ARR 33 ms → 2 ticks; convert Settings ms at load, clamp
  ARR ≥1 tick), so repeat cadence is framerate-independent; suppress emission while
  `RebindingCapture` is active (set by T16). Soft-drop rate multiplier from Settings. Emits
  into `PendingActions`.
- **validation**: Tick-driven tests (no wall-clock sleeps): held key → one press move, then
  first repeat after exactly DAS-ticks, then ARR-tick cadence; release stops; rotate/hold/
  hard-drop single-trigger per press; emission suppressed while `RebindingCapture` set;
  wheel events map to rotations.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T13: HUD
- **depends_on**: [T11]
- **location**: `crates/tetris-app/src/hud.rs`
- **description**: bevy_ui: score/level/lines, next×5 mini-grids (respecting Settings queue
  size 1–6), hold box with used-state dimming, pause-key hint (reflecting the current bound
  pause chord), from `Game::snapshot()` + events.
- **validation**: HUD matches core state during scripted game incl. hold-dim and queue refill;
  survives window resize with letterbox.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T14: M2 vertical slice integration & playtest — **M2 GATE**
- **depends_on**: [T10, T11, T12, T13]
- **location**: `crates/tetris-app/src/`, `PRD.md` (status bump)
- **description**: Wire the full loop to a human-playable slice: spawn→play→lines→level-up→
  game over→restart (R). 180 + hold active in slice per author decision. Fix feel defects
  found while playing (input latency, DAS tuning defaults).
- **validation**: Automated part — scripted headless marathon replay through the wired bridge
  (spawn→lines→level-up→game over→restart) with zero panics and frame-time log ≥58 FPS avg;
  `cargo test/clippy/fmt` green. Human part — agent runs the manual checklist (full game,
  hard/soft drop, hold, 180, ghost, restart feel) in a real window, posts results, and
  **stops for author sign-off**; the gate passes only on author approval.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T15: Settings & best-score persistence
- **depends_on**: [T10]
- **location**: `crates/tetris-app/src/settings_persist.rs`
- **description**: Load/save for the **T1-owned** `Settings` and a `BestScore` serde model →
  JSON at `dirs::config_dir()/<app>/` (no new/divergent settings structs here); load at
  startup with corrupt-file → defaults + warn; atomic save (tmp+rename); debounced save on
  change.
- **validation**: Round-trip unit tests w/ temp dir; corrupt file recovery test; input DAS/ARR
  and next-queue size read from loaded settings.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T16: Settings screen, rebinding, sliders
- **depends_on**: [T12, T15]
- **location**: `crates/tetris-app/src/screens_settings.rs`
- **description**: Settings UI in the `SettingsScreenPlugin` stub (Bevy 0.19 widgets +
  `ValueChange` events, `AutoDirectionalNavigation`): key rebinding capture flow (incl. the
  `Pause` chord — sets/clears `RebindingCapture` so T12 mutes emission during capture),
  master/SFX/music volumes, DAS/ARR, next-queue size (1–6), **effects quality
  Low/Med/High** (PRD §8 "all juice skippable"). Applies live to `Settings`; persists via T15.
- **validation**: Change DAS slider mid-game → repeat cadence changes within one setting
  reload and survives restart; rebind + pause chord persist and rebind takes effect; effects
  quality persists; entering capture blocks gameplay actions (assert via `RebindingCapture`);
  screen content verified via `AppState::Settings` transition test — end-user reachability
  from title/pause is verified in T17 (its gate).
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T17: Title / pause / game-over screens — **M3 GATE**
- **depends_on**: [T14, T15, T16]
- **location**: `crates/tetris-app/src/screens_menu.rs`
- **description**: Menu screen logic in `MenuScreensPlugin` for the T1-owned `AppState`
  (Title, Playing, Paused, Settings, GameOver). Pause via the bound chord from `KeyBindings`
  (never a core `Action`), overlay w/ resume/restart/settings/quit; game-over shows final
  **score, level, and lines** + persisted best (PRD §7.4); hooks T16's settings screen; BGM
  ducking wired to T18's audio API.
- **validation**: All PRD §7 screens reachable by input (incl. title/pause → settings — this
  closes T16's deferred check); sim frozen while paused (zero core ticks, inputs buffered);
  best updates only on game over and displays score+level+lines.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T18: Audio — SFX, BGM, ducking
- **depends_on**: [T10]
- **location**: `crates/tetris-app/src/audio.rs`, `crates/tetris-app/assets/`
- **description**: First sub-step: verify `bevy_kira_audio 0.26.0` compiles against Bevy
  0.19.1 (kira 0.26.0 declares `bevy ^0.19` — compat confirmed pre-spike); if not, fall back
  to built-in audio (`2d` feature) — record choice. Wire core
  events → SFX set (move/rotate/lock/clear/tetris/t-spin/level/hard-drop/hold/game-over),
  looping BGM, per-volume settings, ducking on pause.
- **validation**: Machine-checked with an `EventReader` count against a fixture spawning every
  `GameEvent` variant — exactly one SFX play per event incl. TSpinDetected/HoldPerformed/
  PerfectClear/ComboChanged (no duplicates on re-entrant frames); volumes react to
  programmatic `Settings` changes (slider wiring verified in T16); pause ducks music.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T19: Juice — flash, freeze frames, shake
- **depends_on**: [T11, T15, T18]
- **location**: `crates/tetris-app/src/juice.rs`
- **description**: Line-clear flash + sim freeze via the T10 `SimPaused` bridge gate —
  **≤4 ticks (≤80 ms @ 60 Hz)**, render loop unaffected; on resume, coalesce queued move
  actions to at most one per tick so a backlog can't teleport the piece or burn lock-delay
  resets; hard-drop shake (subtle, decaying), lock flash, ghost alpha; `EffectsQuality`
  Low/Med/High from Settings disables shake/freeze at Low (selector UI in T16).
- **validation**: Freeze never exceeds 4 ticks (asserted in test); input held through freeze
  applies coalesced on resume with correct lock-reset count; Low effects → zero camera motion
  and no freeze; 60 FPS sustained.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T20: CI + release builds — **M5 opener**
- **depends_on**: [T17, T19]
- **location**: `.github/workflows/`, `docs/`
- **description**: GitHub Actions (deferred per author until M5): fmt + clippy `-D warnings` +
  `cargo test` matrix (Linux; Windows/macOS release jobs), tagged releases attach 3-OS
  release binaries. **Nightly job**: `cargo test --release -p tetris-core -- --ignored`
  (T9's wall-clock soak) with failure notifications.
- **validation**: CI green on tag push; artifacts downloadable for ubuntu/windows/macos.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T21: Icon, screenshots, docs polish
- **depends_on**: [T14]
- **location**: `crates/tetris-app/assets/`, `README.md`, `assets/`
- **description**: App icon, window icon; capture screenshots via `EasyScreenshotPlugin`;
  README gameplay section + screenshots; final name decision executed (rename binary/crate
  display name; PRD §14 #1 closes).
- **validation**: `cargo run` shows icon; README renders screenshots; name grep consistent.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T22: v0.1.0 release
- **depends_on**: [T20, T21]
- **location**: repo root (tag), `PRD.md`
- **description**: Version bump, changelog, PRD status → Released v1, tag `v0.1.0`, release
  notes; verify success criteria PRD §12 (playtest cadence, green nightly wall-clock soak run
  from T20's schedule, clippy, fresh-machine `cargo run`).
- **validation**: Tag triggers T20 pipeline; fresh clone + `cargo run` works on clean machine.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T23: WASM build (stretch — M6)
- **depends_on**: [T22]
- **location**: `Cargo.toml` (feature), `index.html`, `.cargo/config.toml`, `crates/tetris-app/src/settings_persist.rs`, docs
- **description**: `trunk`-driven WASM build: Bevy wasm-safe feature subset, `getrandom/js`
  (manifest-metadata exception per T1 rule), asset compression, input focus quirks pass.
  `settings_persist.rs`: `#[cfg(target_arch = "wasm32")]` backend on web storage (or an
  explicit, logged no-op) since `dirs`/fs fail on wasm; wasm audio spike sub-step mirroring
  T18's compat check. Only if M1–M5 accepted.
- **validation**: Playable in Firefox + Chromium at ≥30 FPS; desktop build unaffected
  (`cargo test` still green).
- **status**: Not Completed
- **log**:
- **files edited/created**:

## Parallel Execution Groups

| Wave (earliest) | Tasks | Can Start When |
| --- | --- | --- |
| 1 | T1 | Immediately |
| 2 | T2, T3, T5 | T1 |
| 3 | T4 | T2 |
| 4 | T6, T7 | T4 (T6 also T3) |
| 5 | T8 | T4–T7 |
| 6 | T9, T10 | T8 (T9), T1+T8 (T10) |
| 7 | T11, T12, T15, T18 | T10 |
| 8 | T13, T16, T19 | T11 / T12+T15 / T11+T15+T18 |
| 9 | T14 (**M2 gate**) | T10–T13 |
| 10 | T17 (**M3 gate**), T21 | T14, T15, T16 |
| 11 | T20 | T17, T19 |
| 12 | T22 | T20, T21 |
| 13 | T23 (stretch) | T22 |

## Testing Strategy

- **Core (every merge):** `cargo test -p tetris-core` — table-driven unit tests (SRS kicks,
  scoring rows, gravity boundaries) + integration replay tests (T8) + proptest invariants and
  1M-tick soak (T9).
- **App (every merge):** `cargo clippy --all-targets -D warnings`, `cargo fmt --check`,
  compile-fresh; logic stays in core so app tests stay thin.
- **Manual gates:** M2 checklist in T14, M4 "feels good" pass over T18/T19, M5 fresh-machine
  run in T22. Human gates require **author sign-off**: the agent runs the checklist, posts
  results, and stops — it must not self-certify a gate.
- **Regression discipline:** any rule bug found in playtesting first reproduced as a
  `tetris-core` unit test, then fixed.

## Risks & Mitigations

- **bevy_kira_audio 0.26 ↔ Bevy 0.19 compat** → resolved: kira 0.26.0 requires `bevy ^0.19.0`.
  T18 keeps a short compile spike; fallback to built-in audio already enabled by the `2d`
  feature. Zero schedule impact.
- **Parallel file contention** (agents editing same files) → T1 pre-creates module stubs and
  ALL dependencies; plan assigns exclusive file ownership per task; no shared `Cargo.toml`
  edits after T1.
- **Bevy 0.19 API churn at upgrade time** → pinned minor; upgrades only between milestones,
  core crate keeps it cheap (PRD §13).
- **Freeze-frame vs fixed-step bridge** can eat inputs or double-lock pieces → T19 freezes
  simulation stepping only and buffers actions; covered by its validation clause.
- **SRS 180° behavior is not in original SRS spec** → kick policy explicitly chosen in T4,
  documented there, pinned by table tests.
- **Core→app contract drift** — three app tasks code against T8's events/snapshot → contract
  is declared frozen in Prerequisites and pinned by a T8 snapshot-completeness test; app
  tasks needing contract changes must serialize through a plan amendment, not local edits.
- **Trademark ("Tetris")** → rename executed in T21 before any public release (PRD §14).
