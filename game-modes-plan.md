# Plan: Blockfall Game Modes (PRD: game-modes-PRD.md)

**Generated**: 2026-10-01

## Overview

Add nine game modes, two versus rules and mutators to Blockfall over three
gated releases, per `game-modes-PRD.md`. The keystone is R1 (mode config +
tick clock + per-mode records), which every later mode reuses as data. The
core stays deterministic; times are ticks; the frozen
`Game`/`GameEvent`/`Action` contract reopens exactly once (T2) and
`GameSnapshot`/`MatchSnapshot` wire shapes stay byte-stable until the R3
protocol bump.

**Owner decisions locked in (2026-10-01):**
- Plan covers all three releases, with hard release gates between them.
- Sprint and Dig gravity: **fixed at level 1** (`levels_advance = false`).
- Mutator runs: **no record** (mode play counters still increment).
- Switch: queued garbage **follows the board** (swap exchanges board, hold,
  next queue and pending garbage).
- Race rule: **unchanged** (no same-pieces retrofit).

**Assumptions for the remaining PRD open questions** (flip before their task
runs if the owner decides otherwise):
- Daily Challenge: first completed run of the day is recorded; share line is
  **display-only text** on the result screen (Bevy 0.19 has no clipboard).
- Zen top-out: **wipes the whole stack**.
- `game-modes-PRD.md` check-in beside `PRD.md` (+ §14 item 3 close +
  CHANGELOG): done as the final docs task (T25), pending the owner checkbox.

## Prerequisites

- Rust toolchain per `rust-toolchain.toml` (clippy + rustfmt included).
- Validation loop: `cargo test --workspace`, `cargo clippy --workspace -- -D
  warnings`, `cargo fmt --all --check`.
- Headless drivers already present: `TETRIS_BOT=1`, `TETRIS_1V1=garbage|race`,
  `TETRIS_NET=host:<port>|join:<addr>`, `TETRIS_SEED`, `TETRIS_CONFIG_DIR`,
  nightly `#[ignore]`d soaks (`cargo test --workspace -- --ignored`).
- No new dependencies. No accounts, servers, or >2-player matches (PRD
  non-goals).

## Key architectural constraints (carry through every task)

1. **Wire stability in R1/R2**: do NOT add/reorder fields in `GameSnapshot`,
   `MatchSnapshot`, or `AttackRule` (all bincode-encoded for
   `snapshot_hash`/`MatchStart`). New `GameEvent` variants are safe (events
   never cross the wire) but must be **appended** at the end of the enum.
   The R6 protocol bump happens only in T19.
2. `Game::new(seed)` keeps its exact current behavior; mode behavior arrives
   via `Game::with_config(seed, &ModeConfig)` whose default config reproduces
   Marathon bit-for-bit (gated by the golden test T1).
3. The core never reads a clock or date; the Daily seed is derived in the app.
4. Countdowns (Sprint/Dig 3 s, Ultra warning, Switch swap warning) are tick
   budgets, not wall time. Solo start countdowns gate stepping in the app
   bridge so core tick 0 == first playable frame.
5. `state.rs` T1 contract: only **additive** changes (new `AppState` variant);
   fix non-exhaustive matches it creates.
6. Every task ends with the repo green: tests + clippy `-D warnings` + fmt.

## Dependency Graph

```
Release 1                          Release 2                  Release 3
T1 ──► T2 ──┬──────────────────────────────────────┐
            ├── T3 ── T4 ──┐                       │
            │              ├── T10 ─┐              │
            └── T5 ─┬── T8 ─┼───────┼─┐            │
  T6 ───────────────┼───────┼─── T7 ─┼─ T9 ─ T11 ═ GATE ═╗
                    │       │        │                  ║
                    ╚═══════╧════════╧══╗               ║
                                        ║               ║
                              T12 ── T13║               ║
                              T14 ══════╣               ║
                              T15 ── T16║               ║
                              T17 ══════╩═══ T18 ═ GATE ═╗
                                                         ║
                                   T19 ── T20 ── T21 ─┐  ║
                                   T23 ── T24         ├─ T22
                                                      │     ║
                                   T25 ═ FINAL GATE ◄═╧═════╝
```

## Tasks

### T1: Marathon golden regression test
- **depends_on**: []
- **location**: `crates/tetris-core/tests/marathon_regression.rs` (new)
- **description**: Pin today's behavior before any core change: for 3 fixed
  seeds, run scripted action logs (moves, rotations, drops, holds, ticks to
  top-out), assert exact final `GameSnapshot` field values AND the exact
  bincode byte length + hash of the snapshot (wire-shape canary). This test
  is the R1 release gate's "Marathon unchanged" criterion.
- **validation**: `cargo test -p tetris-core marathon` passes on current code;
  stays green through T2–T11.
- **status**: Completed
- **log**: 2026-10-01 — 3 seeds (31337, 20261001, 42) driven to top-out by a
  deterministic scripted driver using only the public `Game` API: per-piece
  cycle = optional Hold (every 4th) + rotation + column moves + optional
  SoftDrop (every 3rd) + HardDrop + 2 gravity ticks; placements chosen by a
  greedy solver mirroring `core_bridge`'s bot weights, so all 8 `Action`
  variants are exercised per seed and each log is reproducible from the seed
  alone. Pins per seed: scalar snapshot fields (score/level/lines/combo/b2b/
  game_over/hold/hold_used/next), FNV-1a-64 digest over the 10×22 board,
  exact bincode byte length (fixint + reject-trailing codec, mirrored from
  `protocol.rs`) and FNV-1a-64 over those bytes (same loop as
  `snapshot_hash`). Rich goldens: seeds clear 49/164/485 lines, reach levels
  5/17/49, seed 42 ends b2b-armed. `bincode` added as tetris-core dev-dep
  (workspace-inherited); no production code touched. Green: workspace tests,
  clippy `-D warnings`, fmt. Canary proven: flipping seed 42's fnv1a64 by 1
  fails with the Golden diff, restored value passes.
  Gotchas: (a) a purely blind scripted rush tops out but never completes a
  row (~0/100k seeds clear a line) — the greedy placement phase is what makes
  line clears/combo/level/b2b values non-trivial, keep it if regenerating;
  (b) goldens assume the driver's deterministic phase cadence
  (`cycles % 4` hold, `% 3` soft-drop, `% 5` rotation round trip) and the
  solver's tie-break — regenerate all three seeds together if the driver
  ever changes; (c) T2's `Game::new` delegation must reproduce these exact
  values — that diff is the gate signal.
- **files edited/created**: `crates/tetris-core/tests/marathon_regression.rs`
  (new), `crates/tetris-core/Cargo.toml` (+`bincode` dev-dep), `Cargo.lock`

### T2: Core mode config, tick clock, goal/time-up events
- **depends_on**: [T1]
- **location**: `crates/tetris-core/src/mode.rs` (new), `crates/tetris-core/src/game.rs`, `crates/tetris-core/src/event.rs`, `crates/tetris-core/src/lib.rs`
- **description**: The one deliberate reopening of the frozen contract.
  `mode.rs`: `ModeConfig { start_level: u32, levels_advance: bool, goal:
  Option<Goal>, clock_ticks: Option<u64>, start_board: Option<StartBoard>,
  on_block_out: BlockOutBehavior }` with `Default` == today's Marathon
  (level 1, advancing, no goal/clock, empty board, end on block-out);
  `Goal::Lines(u32) | Goal::GarbageCleared`; `StartBoard::BuriedGarbage {
  rows: usize }` (one hole per row, no two adjacent rows share a hole column,
  holes drawn from a splitmix64 stream derived from the seed);
  `BlockOutBehavior::End | WipeAndContinue` (behavior wired in T14, plumbed
  here). `Game::with_config(seed, &ModeConfig)`; `Game::new` delegates with
  the default config. Game internals: `ticks: u64` incremented in `tick()`,
  public `tick_count()`; `levels_advance=false` pins gravity to
  `interval_for(start_level)` and suppresses `LevelUp`; fixed `start_level`
  seeds `gravity::interval_for` without touching `gravity.rs` semantics.
  Terminal handling: reaching goal or clock expiry freezes the game (all
  further `tick`/`apply` are no-ops, like game-over) and emits `GameEvent::
  GoalReached { tick }` / `GameEvent::TimeUp { tick }` — new variants
  **appended** to the enum (T1 wire-canary must stay green: `GameSnapshot`
  gets NO fields; terminal state is exposed via a `Game::finished_reason()
  -> Option<FinishReason>` getter, not the snapshot). `garbage_rows_left()`
  getter counts rows containing ≥1 `Piece::Garbage` cell.
- **validation**: Unit tests in `mode.rs`/`game.rs`: default config ==
  `Game::new` (snapshot equality over scripted play); fixed-level config
  never emits `LevelUp`; `Goal::Lines(40)` emits `GoalReached` exactly once
  and freezes; `clock_ticks: 100` emits `TimeUp` at tick 100 even with no
  pieces placed; buried-garbage start board satisfies the no-adjacent-hole
  invariant over 100 seeds. T1 golden test still passes.
- **status**: Completed
- **log**: 2026-10-01 — commit `0e5e63b`. New `mode.rs`: `ModeConfig {
  start_level: u32, levels_advance: bool, goal: Option<Goal>, clock_ticks:
  Option<u64>, start_board: Option<StartBoard>, on_block_out:
  BlockOutBehavior }` (all serde; `Default` == Marathon), `Goal::Lines(u32)
  | GarbageCleared`, `StartBoard::BuriedGarbage { rows }` +
  `StartBoard::build(seed)` (holes from splitmix64 `seed ^ 0xFF51AFD2AFEDDAB9`,
  redraw-if-equal-previous ⇒ no adjacent-row shared hole; `rows` clamped to
  20 so hidden spawn rows stay empty — first-piece spawn verified live for
  100 seeds; also usable directly by T20 for identical duel boards),
  `BlockOutBehavior::End | WipeAndContinue` (plumbed only — until T14 wires
  the wipe it acts like End), `FinishReason { TopOut, GoalReached, TimeUp }`.
  `game.rs`: `Game::with_config(seed, &ModeConfig)` (`new` delegates with
  the default); `ticks: u64` incremented once per live `tick()` (stops when
  frozen), `tick_count()`; `finished: Option<FinishReason>` is the internal
  terminal flag — `tick`/`apply` gate on `finished.is_some()`; block-out
  sets `game_over` + `finished = TopOut` together (snapshot unchanged),
  goal/clock set `finished` with `game_over == false`. Goal evaluated
  post-line-clear in `lock_and_spawn` (both `Lines` and `GarbageCleared`
  fire — T3 risk reduced), skipping the next spawn (active stays `None`);
  clock evaluated end-of-`tick()` (`ticks >= n`), top-out/goal win when
  concurrent; `levels_advance=false` suppresses level changes + `LevelUp`
  (level pinned ⇒ gravity naturally `interval_for(start_level)` from tick
  0 — verified descent at exactly 12 ticks for level 5). `event.rs`:
  `GameEvent::GoalReached { tick }` / `TimeUp { tick }` **appended**
  (indices 10/11; bincode canary green). New getters: `finished_reason()`,
  `garbage_rows_left()`, `tick_count()`. 16 new tests (bit-identical
  default==new incl. bincode bytes, fixed-level, both goals, clock at tick
  100 with zero pieces, 100-seed buried-board invariants, terminal-reason
  exposure); `cargo test -p tetris-core` 162 passed (marathon golden
  included); RED first captured (E0433/E0599 on unresolved T2 API), then
  GREEN. Gotchas: (a) `tetris-app/src/audio.rs::sfx_for_event` is an
  exhaustive `GameEvent` match — appending the T2 variants broke app
  compilation; a minimal 2-arm silent fix (`GoalReached | TimeUp => return
  None`) is in the working tree but deliberately **unstaged** per the T2
  staging rule — whoever lands the next app-crate commit must include it
  (or T5/T8 re-derive it); proper cue mapping is T8's. (b) `TimeUp` freeze
  keeps the active piece in the snapshot (`TimeUp` ≠ top-out visually);
  goal/top-out freeze with `active == None`. (c) `Goal::GarbageCleared`
  with no garbage at start fires on the first lock — configs pairing it
  must set a buried `StartBoard` (documented on the variant). (d) `ticks`
  does not advance once frozen (a clock-frozen game's `tick_count()` stays
  `== clock_ticks`).
- **files edited/created**: `crates/tetris-core/src/mode.rs` (new),
  `crates/tetris-core/src/game.rs`, `crates/tetris-core/src/event.rs`,
  `crates/tetris-core/src/lib.rs` (+ unstaged: minimal
  `crates/tetris-app/src/audio.rs` compile arm, see log)

### T3: Core Dig mechanics
- **depends_on**: [T2]
- **location**: `crates/tetris-core/src/mode.rs`, `crates/tetris-core/src/game.rs` (+ tests)
- **description**: Make `Goal::GarbageCleared` fire when `garbage_rows_left()`
  hits 0 (evaluate after `lock_and_spawn`'s line clear). Note garbage rows
  shift down with the stack; the metric is rows containing `Piece::Garbage`,
  so clearing any garbage cell in a row retires that row. Sprint config is
  `Goal::Lines(40)`; confirm Sprint needs no further core work (config-only,
  exercised in T4).
- **validation**: Core unit test: seed a buried board, script clears to zero
  garbage ⇒ `GoalReached { tick }`; top-out before that ⇒ `GameOver` only, no
  goal. Same-seed replay equality test for a full Dig run.
- **status**: Completed
- **log**: 2026-10-01 — commit `98b5099`. **T2 fully covered goal firing**:
  `lock_and_spawn` already evaluates `Goal::GarbageCleared` after
  `clear_full_rows()`, freezes and emits `GoalReached` — no production change
  needed (diff is test-only + doc). 6 new tests in `game.rs`: scripted Dig
  clear (full Dig config + hand-made paired well board, public-API driver
  drops Os into the cols 4-5 well / balanced-dumps everything else: 87 steps,
  garbage 10→0, exactly one `GoalReached { tick }` at tick 56, frozen after,
  `finished_reason() == GoalReached`, no GameOver/LevelUp), top-out under the
  Dig config w/ real `BuriedGarbage` board (Z stack + O lock ⇒ `GameOver`,
  `TopOut`, garbage intact, no goal ever), same-seed replay equality (events
  + snapshots + final bincode bytes; seeds 1 vs 2 diverge), `LineCleared`
  strictly-before-`GoalReached` in the final lock's batch (and GoalReached
  ends the batch), retire/shift semantics (lone garbage cell counts; row
  retires via line clear; survivors shift down counted — documents AC5),
  Sprint config `Lines(40)` fires exactly once at 40 lines, level pinned 1,
  never `LevelUp`, freezes ⇒ Sprint needs no further core work. RED-equivalent
  (T2 already correct ⇒ behavior-red impossible): scratch-disabled
  `GarbageCleared => false` ⇒ scripted/replay/batch-ordering tests all fail
  (goal never fires ⇒ driver runs past budget), restored ⇒ green; scratch
  run with disabled firing captured. `mode.rs`: `Goal::GarbageCleared` doc
  now states the retire-a-row-on-any-garbage-cell-cleared semantics.
  GREEN: `cargo test -p tetris-core` 159 lib + 6 invariant + 3 marathon
  golden pass (canary untouched); clippy `--all-targets -D warnings` clean;
  `cargo fmt --all --check` clean.
  **GOTCHA (matters for T4/T10/T20):** a `BuriedGarbage { rows: 10 }` board
  is NOT clearable by any simple scripted or greedy solver. Each row's hole
  is only reachable while that row is the band top (band below is solid
  garbage; only a vertical-I-in-the-hole-column drop completes a 1-hole
  row), and every dig leaves its I's debris directly above the new band top,
  permanently blocking that column for all deeper digs. Clearing all 10 rows
  therefore needs hole columns never reused (~10!/10^10 of seeds) *and* a
  legal place for the ~25+ non-I pieces dealt meanwhile — every column is a
  future descent column, so naive stacks always die (empirical: 0/200k seeds
  win with a top-down vertical-I + hold-fishing driver; balanced dump-zone
  drivers win only against hand-made paired-well boards). Win requires the
  deep line-clearing dig heuristic T10 is charged with building; T4's
  "Dig config completes headlessly" criterion and T20's duel win-path depend
  on it. `GarbageCleared` firing itself is proven correct regardless of the
  start board (hand-made garbage via the test-friendlier `g.board =` path
  used in the AC tests).
- **files edited/created**: `crates/tetris-core/src/game.rs` (tests only),
  `crates/tetris-core/src/mode.rs` (doc only)

### T4: Sprint + Ultra config semantics, headless completion tests
- **depends_on**: [T2, T3]
- **location**: `crates/tetris-core/tests/modes_headless.rs` (new)
- **description**: Pure-core proof (no Bevy) using the greedy solver pattern
  from `core_bridge` reimplemented against `Game` directly, or a scripted
  hard-drop driver: (a) Sprint config (40 lines, fixed level 1) completes with
  `GoalReached`; (b) Dig config (10 buried rows, fixed level 1) completes
  with `GoalReached`; (c) Ultra config (clock 7 200 ticks, marathon
  progression) emits `TimeUp` at **exactly** tick 7 200 with a non-zero score
  and score stands (no top-out needed); top-out before the clock also ends it
  with score kept. These are the PRD's "Solo modes complete headlessly" CI
  criterion; re-run in CI on every push (not `#[ignore]`d).
- **validation**: `cargo test -p tetris-core modes_headless` green in CI.
- **status**: Completed
- **log**: 2026-10-01 — commit `9c6a96c`. New integration test file, test-only
  (no production change). 5 tests, ~0.9 s debug runtime, NOT `#[ignore]`d:
  (a) **Sprint** seed 31 337, greedy hard-drop driver (marathon-solver
  weights) clears 40 lines ⇒ `GoalReached` exactly once at the goal tick,
  `finished_reason() == GoalReached`, level pinned 1, no `LevelUp`/`GameOver`,
  hard freeze, bounded ≤ 6000 steps.
  (b) **Dig** — **REAL `BuriedGarbage { rows: 10 }` boards complete; the
  hand-made fallback was NOT needed.** The nub-down heuristic (fill the
  CURRENT top buried row's hole with a nub-down piece — T nub-down, J/L at
  180°, or a vertical-I foot — clearing exactly that row; greedy dumps; hold-
  fishing when an upcoming/held piece can dig) **won 6 of 40 probe seeds**
  (9, 28, 30, 31, 35, 36 — all six pinned in the test, ~65–441 steps); most
  losses die at 9/10 rows. This SUPERSEDES the T3 GOTCHA ("0/200k seeds win"):
  that was driver-specific — T3's vertical-I digs bury their own debris
  column, the nub-down top-down line does not (documented in the test under
  `DIG-SOLVABILITY`). T10: reuse this heuristic; win rate ~15% ⇒ seed-fish.
  Per seed: exactly one `GoalReached`, `garbage_rows_left() == 0`, no
  `GameOver`/`LevelUp`, frozen.
  (c) **Ultra** seed 42, greedy one-piece-per-30-ticks for marathon
  progression ⇒ `TimeUp { tick: 7200 }` exactly (once), `tick_count() ==
  7200`, score > 0, `game_over == false`, 7201st tick + applies empty, tick
  clock frozen. Companion: blind hard-drop pile-up ⇒ `TopOut` well before
  tick 7200, `GameOver` (no `TimeUp`), score retained > 0, frozen.
  (d) **Replay**: same-seed Sprint × 2 — events, per-cycle snapshots and
  final bincode bytes identical; seed 42 run differs (non-vacuity).
  RED verified: pinning the Ultra tick to 7 199 and seed 23 (a losing dig
  seed) each fail the respective test, restored ⇒ green. GREEN:
  `cargo test -p tetris-core` 159 lib + 6 invariant + 3 marathon golden +
  5 headless pass; `cargo clippy -p tetris-core --all-targets -- -D warnings`
  clean; `cargo fmt -p tetris-core --check` clean (workspace `fmt --all`
  still shows OTHER agents' in-flight tetris-app WIP files — untouched by
  this task).
  Gotchas: (a) `Game::board`/`install_board` are private — an integration
  test CANNOT install a hand-made board, so the documented fallback path
  was infeasible from `tests/` anyway (T3's paired-well fixture only works
  in the crate's own unit tests); the nub-down win makes it moot;
  (b) dig/dump scoring must match the probe exactly to keep the pinned
  seeds winning; (c) greedy survives the 7 200-tick Ultra clock ONLY at
  the one-piece-per-30-ticks cadence — a per-tick-pace driver places ~2×
  more pieces and can top out before the clock.
- **files edited/created**: `crates/tetris-core/tests/modes_headless.rs`
  (new)

### T5: App solo bridge — mode-aware starts, countdown, terminal reasons
- **depends_on**: [T2]
- **location**: `crates/tetris-app/src/core_bridge/mod.rs`, `crates/tetris-app/src/modes.rs` (new)
- **description**: `modes.rs`: `ModeId` enum (`Marathon, Sprint, Ultra, Dig`
  now; later modes add their variant in their own task) with one catalogue
  `fn mode_config(id) -> ModeConfig` + display name + one-line description,
  so menus/records/catalogue share a single source. `GameCore` gains
  `start_mode(seed, ModeConfig)` (keeping `restart_with` for Marathon and
  honoring `TETRIS_SEED` like `restart_run`); `restart_on_r_system` retries
  the current mode, not raw Marathon. A `Countdown` resource gates stepping
  for modes with a pre-roll (Sprint/Dig: 180 ticks, HUD shows 3-2-1 via
  `SimPaused`-style hold that still drains events); core tick 0 therefore
  equals the first playable frame. On `GoalReached`/`TimeUp` events the
  bridge flips `AppState::GameOver` (terminal reason exposed for T9 via
  `Game::finished_reason()`). **Countdown vs pause**: the pre-roll is its own
  `Countdown(u32)` resource consumed in `FixedUpdate` only while
  `AppState::Playing` and `SimPaused(false)`; it does NOT reuse `SimPaused`
  (a pause+resume via `resume_game` in `screens_menu.rs` must never cancel
  the remaining 3-2-1). Actions arriving during the countdown keep being
  held in `PendingActions` exactly like a freeze frame. Also **pre-declare
  the full `ModeId` catalogue now** (all ten modes with configs, names,
  descriptions) behind an `is_shipped(id) -> bool` list, so later mode tasks
  add no catalogue rows — they only flip `is_shipped` — which keeps
  `modes.rs` free of same-wave edit conflicts.
- **validation**: Headless bridge tests (MinimalPlugins pattern already in the
  file): starting Sprint runs the countdown before the first core tick; Ultra
  `TimeUp` flips `AppState::GameOver`; Sprint top-out yields no goal event.
- **status**: Completed
- **log**: 2026-10-01 — commit `ce46eee`. `modes.rs`: ALL TEN `ModeId`s
  pre-declared (+ `ModeId::ALL`); `mode_config` (Marathon = `default`;
  Sprint = `Lines(40)` + fixed level 1; Ultra = clock 7200 + marathon
  progression; Dig = `GarbageCleared` + `BuriedGarbage{10}` + fixed level 1;
  Survival = placeholder `default` until T12's feed field; Zen = fixed
  level 1 + `WipeAndContinue` (wired T14); BotLadder/Daily/DigDuel/Switch =
  placeholder configs — versus campaigns/rules start via `start_versus`/
  `AttackRule`, and Daily resolves at runtime to Sprint/Ultra/Dig in T17);
  `display_name`/`description` per mode; `is_shipped` = Marathon/Sprint/
  Ultra/Dig only (later tasks flip only this); `pre_roll_ticks` = 180 for
  Sprint/Dig, 0 else; `mode_key` maps all ten onto `records::ModeKey`;
  `format_time_ticks` = `m:ss.hh`, 60 Hz floor division, centiseconds
  **truncated**. `core_bridge`: `ActiveMode { id, config }` lives as a
  **`GameCore` field** (the design-note's non-send option — one new resource
  entity only, no borrow conflicts; `restart_with` resets it to Marathon so
  legacy `restart_run` stays consistent); `GameCore::start_mode(seed, id)` =
  `Game::with_config` + steps/seed/events reset; free fn
  `start_mode_run(id, core, countdown, app_state, Option<&mut Records>) ->
  seed` honors `TETRIS_SEED` like `restart_run`, re-arms the mode's
  pre-roll and `bump_plays` centrally; `Countdown(u32)` resource +
  `countdown_system` (FixedUpdate, before `core_bridge_system`) decrements
  only while `Playing && !SimPaused` — pause freezes the remaining budget,
  never cancels; `PendingActions` held through the pre-roll exactly like
  freeze frames, core tick 0 = first playable frame; `core_bridge_system`
  steps only while `Countdown == 0` and flips `AppState::GameOver` on
  `GoalReached`/`TimeUp` like `GameOver` (CoreEvent forwarding unchanged);
  `restart_on_r_system` retries `core.active_mode.id`; `bot_drive_system`
  idles during a pre-roll.
  **Pre-roll frame math for T7/T8**: 180 ticks = 179 fully-frozen frames +
  the frame whose decrement exhausts the budget, which is core tick 0 (the
  first playable frame); HUD 3-2-1 = `ceil(countdown.0 / 60)`; playable iff
  `Countdown == 0`; terminal reason via `core.game.finished_reason()`.
  RED captured (E0583 `modes` + 7×E0425 `ActiveMode`/`Countdown`/
  `start_mode_run`) → GREEN: `cargo test -p tetris-app` 377 pass,
  `cargo test --workspace` green (T1 golden included), clippy
  `-D warnings` clean, `fmt --check` clean.
  Gotchas: (a) the plan's example "10 235 ticks → 2:43.91" is off by 400 —
  at 60 Hz truncation 10 235 is **2:50.58**, and **9 835** ticks is what
  yields 2:43.91 (both pinned in `format_time_ticks` tests; T8 must not
  assert 2:43.91 against 10 235); (b) the two screens_menu submenu-click
  tests (`one_v_one_submenu_clicks…`, `dummy_resources_cannot_reroute…`)
  are entity-iteration-order sensitive — `rect_of("settings")` grabs the
  first match, which at HEAD is the *pause* panel button (y=494) that
  happens to be inert; measured sensitivity: +1 new `init_resource` green,
  +2 red, +3/+4 still red. T5 passes only because it kept new resource
  entities to exactly one (`Countdown`). **T7/T8 add more resources and
  WILL break them** — fix deterministically in T7's screens_menu pass (make
  `button_rects`/`rect_of` sort by (label, y) and pick the intended root);
  do not work around by avoiding resources. (c) `screens_menu` Start/Retry
  still call `restart_run` (Marathon) — switch them to `start_mode_run` in
  T7/T9; until then `active_mode` self-heals (reset to Marathon by
  `restart_with`). (d) `start_mode_run` returns the resolved seed. (e) the
  Ultra headless test runs the full 7 200-step clock (~1 s).
- **files edited/created**: `crates/tetris-app/src/modes.rs` (new),
  `crates/tetris-app/src/core_bridge/mod.rs`, `crates/tetris-app/src/lib.rs`
  (`mod modes;`)

### T6: Per-mode records file + best.json migration + play counts
- **depends_on**: []
- **location**: `crates/tetris-app/src/records.rs` (new), `crates/tetris-app/src/settings_persist.rs`, `crates/tetris-app/src/screens_menu.rs` (best label call sites)
- **description**: Replace the single `best.json` shape with a per-mode
  records file reusing this module's atomic-write/load discipline:
  `Record { BestTime{ticks}, BestScore{score,level,lines}, LifetimeLines,
  HighestRung, Daily{date,result} }` keyed by `ModeId` string; plus per-mode
  play counters (PRD success metric). Migration: old `{score,level,lines}`
  shape loads into the Marathon `BestScore` and is rewritten in the new
  shape on first save (serde-tagged enum or versioned wrapper; corrupt file
  still falls back to defaults like today). `PersistedBestScore` resource
  stays (title screen reads it) but becomes a view over the Marathon entry.
  **Single-writer fix (critical):** today
  `settings_persist::best_score_system` force-records *every*
  `GameEvent::GameOver` into `PersistedBestScore` — with modes live that
  would record Sprint/Dig/Survival top-outs as Marathon bests (PRD: no
  result) while Ultra's `TimeUp` records nothing. This task retires that
  system as a writer: `Records` becomes the sole recorder, the legacy
  `best_score_system` is reduced to refreshing the `PersistedBestScore` view
  from the Marathon entry (no disk writes), and the game-over screen (T9) is
  the only caller of `record_run`, gated by terminal reason × mode (Sprint/
  Dig top-out → nothing; Ultra either ending → score; Survival GameOver →
  time). API: `Records` resource, `record_for(mode)`, `record_run(mode,
  outcome)` returning `bool` is_record, `bump_plays(mode)`.
- **validation**: Round-trip tests under `TETRIS_CONFIG_DIR` (env-lock
  pattern already in `settings_persist`): legacy file migrates to Marathon;
  corrupt file ⇒ defaults; time/score/rung/daily record updates; play counters
  increment. Existing `app_boots_and_game_over_persists_best_score` adapted.
- **status**: Completed
- **log**: 2026-10-01 — new `records.rs`: `Records` resource (per-mode
  `Record` + per-mode play counts, `BTreeMap` keyed by `ModeKey = &'static str`
  with constants `MARATHON, SPRINT, ULTRA, DIG, SURVIVAL, ZEN, BOT_LADDER,
  DAILY, DIG_DUEL, SWITCH` + `ALL_MODE_KEYS`), internally-tagged
  `#[serde(tag = "kind", rename_all = "snake_case")]`
  `Record { BestTime{ticks}, BestScore{score,level,lines}, LifetimeLines{total},
  HighestRung{rung}, Daily{date,result} }`, API
  `record_for(key) -> Option<&Record>`, `record_run(key, record) -> bool`
  (time: lower wins; score/lifetime/rung: higher wins; daily: latest differing
  content; cross-variant replaces), `bump_plays(key) -> u64`,
  `plays(key) -> u64`, `add_lifetime_lines(n) -> u64` (saturating, Zen).
  `best.json` wrapper `{"version":1,"records":{...},"plays":{...}}`; unknown
  record kinds/fields skipped per-entry (forward compat); load = raw-JSON
  dispatch: top-level `score` ⇒ legacy migrate to Marathon `BestScore`,
  else versioned shape; corrupt ⇒ defaults + `warn!`, never panics. A pure
  load never writes (downgrade safety). Persistence reuses
  `settings_persist::{write_atomic, SAVE_DEBOUNCE_SECS, CONFIG_DIR_ENV}` +
  `RecordsSaveQueue` (debounce/force, force-flush on improved Marathon
  game-over, flush on `AppExit`); `RecordsPlugin` (init resources, Startup
  load, Update flush, Last exit flush) mounted from `SettingsPersistPlugin`.
  **Single-writer fix done:** `save_to`/`save_once`/`flush_system`/
  `exit_flush_system` write settings.json only (`save_to`'s `best` param kept,
  ignored); `best_score_system` now calls `Records::record_run(MARATHON, …)`
  from the `GameCore` snapshot on `CoreEvent(GameOver)` and forces a records
  flush only when improved; new `best_score_view_system` refreshes
  `PersistedBestScore` from the Marathon entry (view only, assign-on-difference);
  `screens_menu.rs` untouched (API compatible). `records::save_to` is the
  only `best.json` writer. Tests: legacy-migration/never-rewritten-on-load,
  corrupt/missing ⇒ defaults, unknown-kind tolerance, per-variant improvement
  rules, saturating lifetime lines, play counters, read-only-boot writes
  nothing, exit flush, view tracks record and never regresses, settings exit
  flush never creates best.json. GREEN: `cargo test -p tetris-app` 364 pass,
  workspace green, clippy `-D warnings` clean, fmt clean. Validation done in
  a clean `git worktree` at HEAD because another agent's WIP core edits
  (`GoalReached`/`TimeUp`) broke the shared tree's `audio.rs` mid-task —
  verify against the merged tree too.
  Gotchas for T7/T9/T14/T16/T17: (a) T9 gates `record_run` per mode × terminal
  reason and should also `bump_plays` — T6 deliberately does NOT bump plays on
  game-over (would double-count); (b) only Marathon auto-records via the interim
  `best_score_system` — remove it in T9 when the result screen calls
  `record_run` directly, and gate it so non-Marathon game-overs don't write
  the Marathon record; (c) `add_lifetime_lines` keys off `ZEN`; (d) env-mutating
  tests must hold `settings_persist::ENV_LOCK` (historical flake); (e) legacy
  files are rewritten to the new shape only on first real save, and legacy
  `{score,level,lines}` is detected by the top-level `score` key — the new
  wrapper must never put `score` at top level.
- **files edited/created**: `crates/tetris-app/src/records.rs` (new),
  `crates/tetris-app/src/settings_persist.rs`, `crates/tetris-app/src/lib.rs`
  (`mod records;`) — `screens_menu.rs` NOT touched (view kept API-compatible)

### T7: Mode select screen (portrait-first)
- **depends_on**: [T5, T6]
- **location**: `crates/tetris-app/src/screens_modes.rs` (new), `crates/tetris-app/src/screens_menu.rs`, `crates/tetris-app/src/state.rs`, `crates/tetris-app/src/touch.rs`
- **description**: Add `AppState::ModeSelect` (additive; fix exhaustive
  matches). Title's "Start" now goes to a scrolling list: one row per solo
  mode (Sprint/Ultra/Dig + Marathon), each showing name, one-line description
  and its record from T6. Row press → `start_mode` via T5. Keep the existing
  1 v 1 / Online / Settings / Quit buttons. Reuse the `menu_button` marker /
  `Interaction` click pattern; must be touch-operable and readable in portrait
  (scroll container; PRD risk "ten entries crowd the phone menu").
- **validation**: Headless UI test: button click moves `AppState` to
  `ModeSelect`, row click starts the right `ModeConfig`; manual portrait APK
  check (start, scroll, select, back).
- **status**: Completed
- **log**: 2026-10-01 — commit `3a6f486` (`screens_modes.rs` new,
  `screens_menu.rs`, `state.rs`, `lib.rs`; `touch.rs` intentionally
  untouched — see touch note). `AppState::ModeSelect` added after `Title`;
  all existing `match` sites have wildcards (nothing broke). New
  `ModeSelectPlugin` mounted after `MenuScreensPlugin`: root spawned once
  (hidden like the T17 menu roots, handlers state-gated), rows rendered from
  `ModeId::ALL` filtered by `is_shipped` (never hardcoded) via
  `spawn_mode_rows`; each row = `ModeRowButton { id }` + display name +
  description + `record_line` refreshed live on `Records::is_changed()`.
  Record formats (pure fns `record_line`/`group_thousands`): `-`,
  `Best 2:43.91` (`format_time_ticks`), `Best 123 456` (space thousands),
  `Lines N`, `Rung N`, `date: result` — ASCII only (bundled font subset).
  Row press → `open_mode_select`-pair helper `start_mode_row` → shared
  `start_mode_run` (seed/pre-roll/plays) — Sprint row test asserts pre-roll
  180 + gate. **Scrolling**: native bevy_ui `overflow: Clip/Scroll` +
  `ScrollPosition`; `mode_scroll_system` drives it from
  `MessageReader<MouseWheel>` (90 px/notch) and vertical per-finger touch
  drags (`MessageReader<TouchInput>`), clamped by pure `clamp_scroll`
  against `ComputedNode.size/content_size`; `ui_focus_system` clipping makes
  scrolled-out rows inert (verified). **MessageReader is load-bearing**:
  bevy_ecs 0.19 `Messages::update` only swaps buffers when the resource
  changed, so `iter_current_update_messages` re-delivers or drops messages
  written between updates (bevy_ecs docs: arrival "unpredictable") — a
  wheel test written that way flaked ~50 % until converted; same codebase
  pattern as `touch.rs`/`input.rs`, do NOT "simplify" back. Keyboard: Esc →
  Title, Enter → first shipped row; pause chord already gates on
  Playing/Paused (verified inert). **screens_menu test-helper fix (T8
  canary)**: `button_rects` now returns root-scoped
  `(root, label, pos)` sorted via `total_cmp`, `rect_of_under(root, label)`
  replaces first-match `rect_of`; new churn test
  `root_scoped_rect_resolution_survives_resource_and_entity_churn` — the
  two submenu canaries now survive the extra-resource churn T8 proved was
  order-flipping. Title "Start" → `ModeSelect`; adapted tests:
  `start_button_launches_fresh_playing_run` clicks the Sprint row (pre-roll
  asserted), versus quit/winner-menu flows land on `ModeSelect`. **Touch
  parity**: no `touch.rs` change needed — `ui_focus_system` presses buttons
  from `Touches` fed by `Messages<TouchInput>`; two headless portrait
  411×731 tests tap a row and Back through the real winit-shaped pipeline.
  Manual portrait APK check deferred to the T11 gate (same as T8's).
  Gotchas: (1) Pause "Restart" and Game-Over R still use `restart_run`;
  confirm in T9 they resume the mode selected here, not Marathon. (2)
  Pre-existing flakes observed during validation, NOT from this tree:
  `core_bridge::tests::ultra_time_up_flips_game_over_and_exposes_terminal_reason`
  (~1–5 %, load-correlated, reproduces on pristine `c3e42ff` at 1/60
  isolated without this commit) and
  `core_bridge::net::gateway::tests::real_gateway_host_registers_and_is_announced`
  (real UDP, flakes under CPU contention) — worth a core_bridge owner's
  look before the T10 CI gates bake them in. Verified: workspace green,
  app 404/404 ×5 on `c551afd` + these changes, wheel test 15/15, clippy
  `--workspace --all-targets -D warnings` + fmt `--check` clean.
- **files edited/created**: `crates/tetris-app/src/screens_modes.rs` (new),
  `crates/tetris-app/src/screens_menu.rs`, `crates/tetris-app/src/state.rs`,
  `crates/tetris-app/src/lib.rs`

### T8: HUD clock and goal counter
- **depends_on**: [T5]
- **location**: `crates/tetris-app/src/hud.rs`, `crates/tetris-app/src/audio.rs`
- **description**: Add a clock label and per-mode goal counter to the solo
  HUD, shown only when the mode requests it (`ModeId` catalogue flag):
  Sprint = count-up clock (mm:ss.hh from `Game::tick_count()`), lines left,
  pieces placed (count `PieceLocked` events client-side); Ultra = count-down
  from `clock_ticks` (score already shown); Dig = clock + garbage rows left
  (`garbage_rows_left()`). Tick→time formatting helper lives in `modes.rs`.
  **Data carrier**: `GameSnapshot` gains no fields (wire rule) and
  `HudFixture` only injects snapshots — so the bridge writes a
  `ModeHudInfo` resource (clock ticks, goal text inputs: lines-left /
  garbage-left / pieces-placed, feed queue + next-row countdown for T13,
  swap-timer field reserved for T21) every fixed step from the core getters;
  HUD systems read `ModeHudInfo`, and extend `HudFixture` to also accept it.
  Ultra: warning sound in the last 10 s (one shot at tick 6 600 via
  comparison against `ModeHudInfo`, using existing `bevy_kira_audio`
  WAV assets; if no fitting asset exists, reuse the most urgent existing cue).
- **validation**: Headless HUD tests (fixture pattern `HudFixture` already
  exists): label text formats ticks correctly (e.g. 9 835 → `2:43.91`;
  10 235 → `2:50.58` — 60 Hz truncation);
  marathon run shows no clock; ultra warning fired-once behavior.
- **status**: Completed
- **log**: 2026-10-01 — commit `725b5b8` (`core_bridge/mod.rs`, `hud.rs`,
  `audio.rs`). `ModeHudInfo` resource (the task's **single** new resource,
  `init_resource` in `CoreBridgePlugin`): `{ mode_id, clock_ticks,
  clock_limit, countdown, lines_left, garbage_left, pieces_placed, show_hud,
  feed_pending, feed_next_row_in, swap_in }` — last three reserved `None`
  (T13 feed / T21 swap); `show_hud = goal.is_some() || clock.is_some()` so
  Marathon renders neither row. `mode_hud_refresh_system` (FixedUpdate,
  **after** `core_bridge_system`) refreshes from `tick_count()`/
  `garbage_rows_left()`/`Countdown` every step; `pieces_placed` counts
  `PieceLocked` from `MessageReader<CoreEvent>`, reset by watching
  `GameCore::steps == 0` (both start paths zero steps before the first step
  and the system runs post-step, so the first playable frame reports 1 —
  documented inline). HUD: three new `HudTextSlot`s (`Clock` = `TIME\nm:ss.hh`
  via `modes::format_time_ticks`, count-down when `clock_limit` present;
  `Goal` = `Lines left: N`+`Pieces: P` (Sprint) / `Garbage: N` (Dig) / absent
  (Ultra); `Countdown` = big centered `ceil(n/60)`, clock/goal hidden while
  pre-roll runs). Layout: landscape = right panel below the next queue
  (below even a 6-slot queue, above window bottom); portrait = deck row outer
  margins; 3-2-1 centered over the field in both — never overlapping.
  Fixture pattern extended: tests write `ModeHudInfo` directly + run `Update`
  only (bridge refresh lives in `FixedUpdate`, so fixtures are never
  clobbered; `HudFixture` snapshot usage unchanged/back-compat). **Ultra
  audio decision**: discovered shipped cues are all generated placeholder
  blips (`assets/generate.py`); none reads as a warning and `GameOver`'s
  440→110 Hz sweep would falsely signal run-end at T-10s → edge-detected
  system on `ModeHudInfo` (remaining ≤ 600 crossing, Local-tracked, re-arms
  on restart) fires `info!("ULTRA warning…")` + `SfxDirector::
  note_edge_cue("ultra-warning")` counter; **manual_check**: swap in a real
  warning WAV (`pending_sfx.push_back(…)`) when one ships. Gotchas: (1)
  PROVEN at `c3e42ff` — adding ANY single resource entity (even a dummy, at
  any init position, even lazily on first Update — all 10 positions probed)
  flips the two `screens_menu` submenu-click canaries via `button_rects`/
  `rect_of` first-match-by-iteration-order luck; fix lives in T7's
  deterministic-helper commit (sorted `button_rects`, root-scoped
  `rect_of_under` — verified green WITH my resource when combined in the
  shared tree; my commit alone on `c3e42ff` leaves those two red until T7
  lands — integration ordering, not T8 code). (2) Lazy first-Update resource
  insertion additionally breaks net `esc_on_listening_clears_the_upnp_state`
  — keep build-time `init_resource`. (3) 1 pre-roll tick (180) → display 3,
  but post-pre-roll first playable frame renders `0:00.01` (tick 1), not
  `0:00.00`. Verified: app 404/404 + workspace green with T7 tree helpers
  (worktree-pinned evidence in T8 session report); clippy `-D warnings` +
  fmt `--check` clean.
- **files edited/created**: `crates/tetris-app/src/core_bridge/mod.rs`,
  `crates/tetris-app/src/hud.rs`, `crates/tetris-app/src/audio.rs`

### T9: Mode-aware result (game-over) screen
- **depends_on**: [T5, T6, T8]
- **location**: `crates/tetris-app/src/screens_menu.rs` (game-over screen code)
- **description**: The game-over screen renders per terminal reason and mode:
  Sprint/Dig completed → final time (mm:ss.hh) + "New record"/best time;
  Sprint/Dig top-out → explicitly *no result* shown; Ultra → score stands
  either way + record line. Retry re-runs the same mode (seed fresh unless
  `TETRIS_SEED`); "Menu" returns to `AppState::ModeSelect`. Uses
  `Game::finished_reason()` + `Records::record_run` (records write only when
  the PRD says a result exists: top-out in Sprint/Dig records nothing).
- **validation**: Headless tests per terminal reason (drive core to each
  outcome); record written/not-written assertions through `Records`.
- **status**: Completed
- **log**: `5161a83`. Single terminal recorder now owns all result writes:
  `terminal_record_system` (screens_menu, Update, BEFORE `menu_button_clicks`
  so a same-frame Retry can't skip it) fires once per GameOver entry
  (`state.is_changed()` gate; bridge writes state in FixedUpdate so the next
  Update sees it), folds the result via the pure table
  `terminal_record(ModeId, FinishReason, &GameSnapshot, ticks) ->
  Option<Record>` — `(Marathon, TopOut) | (Ultra, TimeUp|TopOut) =>
  BestScore{score,level,lines}`; `(Sprint|Dig, GoalReached) =>
  BestTime{game.tick_count()}`; `_ => None` — latches
  `TerminalResult{mode, reason, ticks, new_record}` for the display layer,
  and on improvement force-flushes `RecordsSaveQueue` (improved results hit
  `best.json` within one frame; `best_score_view_system` keeps the title
  Marathon view following). **T13 extension = one row**:
  `(ModeId::Survival, FinishReason::TopOut) => Some(Record::BestTime { ticks })`.
  Headline `result_text()`: Sprint/Dig GoalReached → `Time m:ss.hh`
  (`format_time_ticks`), Sprint/Dig TopOut → `No result`, Ultra → `Time up`
  / `Top out`, Marathon → empty; rendered by a new `ResultText` label whose
  `Node.display` toggles `Flex/None`, so Marathon's screen keeps today's
  exact layout. Best line per mode: non-Marathon GameOver reads the mode's
  own `record_line` from `Records`; title/Marathon keep the
  `PersistedBestScore` view verbatim. Retry: `start_new_run` → `retry_run`
  (both GameOver Play-again and pause Restart now route through
  `start_mode_run(active_mode.id)` — same mode, fresh seed unless
  `TETRIS_SEED`, pre-roll re-armed; T7 board note honored). GameOver "Menu"
  → `open_mode_select` (not Title). **Retired** interim
  `settings_persist::best_score_system` (function + registration): it folded
  EVERY GameOver into the Marathon `BestScore`, so Sprint/Dig top-outs
  polluted the Marathon record — RED evidence
  `screens_menu::tests::sprint_top_out_records_nothing_anywhere` failed
  pre-fix with "a Sprint top-out must not touch the Marathon record (PRD: no
  result)" on the full persistence tree, green post-retirement (plus
  `settings_persist::tests::game_over_writes_no_record_from_settings_tree`
  as the permanent gate: game over ⇒ no `Records` entry, no `best.json`).
  Marathon disk regression kept green end-to-end
  (`marathon_top_out_persists_best_score_to_disk`). Verified: app 415/415,
  workspace 14 suites green, clippy `--workspace --all-targets -D warnings`
  + fmt `--check` clean at the commit boundary (HEAD-pinned worktree + the
  two files; shared-tree run incl. parallel T10 WIP also 418/418 green).
- **files edited/created**: `crates/tetris-app/src/screens_menu.rs`,
  `crates/tetris-app/src/settings_persist.rs` (retirement of the interim
  recorder only — it lived there and owned the pollution)

### T10: Release-1 CI completion gates
- **depends_on**: [T3, T4, T5]
- **location**: `crates/tetris-app/src/core_bridge/mod.rs` (bot mode + solver), `.github/` (if job list needs the new test names)
- **description**: Two parts. (a) **Dig-aware solver**: the existing greedy
  `bot_move` weights cover the stack and will bury Dig's holes rather than
  dig them (it tops out well before clearing 10 garbage rows). Give the bot a
  Dig-aware heuristic (e.g. weight the lowest reachable hole strongly when the
  board contains `Piece::Garbage`, target the hole column) without changing
  marathon behavior (marathon boards have no garbage ⇒ heuristic inactive).
  (b) Extend `TETRIS_BOT=1` to run a named mode (`TETRIS_BOT=sprint|ultra|dig`,
  plain `=1` stays marathon), logging `BOT mode_done mode=sprint
  time_ticks=…` / `mode=ultra score=…` / `mode=dig time_ticks=…`, and —
  crucially — `BOT mode_abort mode=… ticks=…` with nonzero-exit semantics on
  top-out, so a silent early death fails the test instead of hanging the
  budget. Ultra: top-out before tick 7 200 still yields `mode_done` (score
  stands either way per PRD). Add one workspace-level hidden-window
  integration test running Sprint and Dig to completion (PRD: "the bot
  finishes Sprint and Dig"); Ultra's exact-tick end is already covered purely
  in core by T4.
- **validation**: `cargo test --workspace` green; `TETRIS_BOT=sprint cargo run`
  (desktop) completes a Sprint run headless-ly and exits 0.
- **status**: Completed
- **log**: `2d4a4b9`. **(a) Dig-aware solver:** ported T4's nub-down
  heuristic verbatim into `core_bridge` (`bot_move_mode(snapshot,
  dig_aware)` + `top_buried`/`dig_landing`/`dump_landing`/
  `dig_score_landing`/`dig_landings` + hold-fish via a new `BotMove::hold`
  one-shot the shared executor pushes as `Action::Hold`). Coexistence: the
  legacy `bot_move` (weights/tie-breaks untouched) stays the *only* path for
  `!dig_aware` **and** for `dig_aware` boards without a `Piece::Garbage`
  cell, so Marathon/Sprint/Ultra decisions are bit-identical — pinned by
  `dig_heuristic_is_inert_without_garbage_cells` (100+ sampled marathon
  decisions, `bot_move_mode(snap,true) == bot_move(snap)`) and by the
  unchanged versus path (`bot_side_drive` always calls with `dig_aware =
  false`; `bot_drive_system` only opts in when
  `active_mode.config.goal == GarbageCleared`). Unit proof of activation:
  `dig_heuristic_targets_top_buried_hole_on_garbage_boards`.
  **(b) Named modes:** `parse_bot_value` accepts
  `1|marathon|sprint|ultra|dig` (case-insensitive; unknown ⇒ bot off +
  warn); named bots start from Title via `start_mode_run` (TETRIS_SEED +
  pre-roll honored). Exactly one machine line per terminal: `BOT mode_done
  mode=sprint time_ticks=…` / `mode=dig time_ticks=…` (GoalReached,
  `AppExit::Success`), `BOT mode_done mode=ultra score=… ticks=…` (TimeUp
  OR TopOut — score stands), `BOT mode_abort mode=… ticks=…` for Sprint/Dig
  top-out with **`AppExit::error()`** nonzero exit (Bevy 0.19 renamed
  `AppExit::Failure` → `Error(NonZero<u8>)`; `AppExit::error()` is the
  nonzero variant). Ultra paced at one hard drop per ~30 ticks
  (`ULTRA_BOT_PACE_TICKS`, T4's proven cadence — free-run greedy tops out
  before the clock). Lifecycle runs inside `bot_marathon_system` as a
  **plain function, not a new registered system**: registering any
  additional `Update` system perturbs the schedule enough to flip the
  netplay UI fixtures' same-frame edge tests
  (`esc_on_listening_clears_the_upnp_state` deterministically failed with a
  4-system tuple; graph-shape-preserving branch passes 4/4).
  **(c) `tests/bot_modes.rs`** (hidden-window-style MinimalPlugins +
  `LogPlugin::custom_layer` info!-capture, env-mutex serialized, 1.7 s):
  Sprint 31337 → `BOT mode_done mode=sprint time_ticks=522` observed, exit
  Success; **all six T4 pins 9/28/30/31/35/36 win through the app wiring**
  (probe seeds 1–40 through the *app*: 6/40 wins — exactly T4's core win
  set, 0 stalls → gravity + step-executor + hold mechanics preserve the core
  driver's outcomes); seed 5 pinned as the abort seed (34/40 seeds top out,
  abort line + `AppExit::error` asserted); ultra survives past 1200 ticks
  with ≤ 60 pieces (cadence proof) and no terminal line; `=1`/`=marathon`
  boot-to-Title regression; unknown value inert + warns. RED evidence: all
  7 tests failed pre-impl (named env values never started a mode; captured
  logs stopped at `start_mode … (from TETRIS_SEED)` with no terminal line);
  GREEN after. Verified: app 418/418, workspace all suites green, clippy
  `--workspace --all-targets -- -D warnings` + fmt `--check` clean (HEAD-
  pinned worktree + the two app files; shared-tree run incl. parallel T9
  WIP also green). Deviation: `lib.rs` `mod` → `pub mod` for
  `core_bridge`/`modes`/`records`/`state` (binary-only crate; required for
  any `tests/` integration test to reach the bridge).
- **files edited/created**: `crates/tetris-app/src/core_bridge/mod.rs`,
  `crates/tetris-app/src/lib.rs` (visibility only),
  `crates/tetris-app/tests/bot_modes.rs` (new)

### T11: RELEASE 1 GATE
- **depends_on**: [T7, T8, T9, T10]
- **location**: repo-wide
- **description**: `cargo test --workspace` + `cargo clippy --workspace -- -D
  warnings` + `cargo fmt --all --check` green (T1 golden proves Marathon
  unchanged — PRD gate). Manual: Android APK portrait check — every R1 mode
  started, played, left with touch only (PRD G5). Playtest Sprint/Ultra/Dig;
  tune any "starting value" constants. Old `best.json` on a real profile
  migrates and the title screen still shows the Marathon best.
- **validation**: All of the above recorded in the plan log. No R2 task starts
  before this is green.
- **status**: Completed
- **log**: R1 automated gate GREEN at commit e5c2411 (2026-10-01). `cargo test
  --workspace` = 15/15 suites ok (0 failures), incl. `marathon_golden_snapshot_*`
  canary (3 passed) proving Marathon unchanged through T2-T10, plus new
  T10 `bot_modes.rs` headless gates (Sprint `mode_done time_ticks=522`, Dig on
  pinned seeds {9,28,30,31,35,36}, Ultra cadence, abort path => `AppExit::error`).
  `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo fmt --all --check` clean. Legacy `best.json` migration + single-writer
  pinned by T6/T9 tests. MANUAL (deferred to owner, cannot run headless here):
  Android APK portrait touch check per R1 mode; Sprint/Ultra/Dig playtest to
  tune `PRE_ROLL_TICKS`, 1800-tick decay consts; real-profile migration on device.
  Known pre-existing flakes (not R1 regressions): `ultra_time_up` ~1-5% under
  load; `real_gateway_host_registers_and_is_announced` under UDP contention.
- **files edited/created**: (gate — no code; this plan entry only)

### T12: Core Survival garbage feed
- **depends_on**: [T11]
- **location**: `crates/tetris-core/src/mode.rs`, `crates/tetris-core/src/game.rs` (+ tests)
- **description**: `GarbageFeed { interval_ticks: 300, decay_ticks: 1800,
  decay_by: 15, floor_ticks: 60 }` (named constants, tunable) in
  `ModeConfig`. Game-internal: feed timer queues 1 row per interval; the
  whole pending batch (cap `versus::MAX_GARBAGE_PER_LAND` = 4, surplus
  trickles like versus) lands on the player's next lock by reusing
  `versus::push_garbage_rows` (already `pub(crate)`); hole column per batch
  from an independent splitmix64 stream derived from the seed (never the bag
  stream — bag draws must stay untouched). Overflow past the ceiling tops out
  exactly like a block-out. Expose `pending_garbage()` and `ticks_to_next_row`
  getters for the HUD.
- **validation**: Unit tests: first row lands on the first lock after tick
  300; interval decays 15 per 1800 ticks down to the 60 floor; cap-4 trickle;
  overflow tops out; same-seed replay determinism; marathon regression (T1)
  untouched.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T13: Survival app wiring + HUD
- **depends_on**: [T12, T8]
- **location**: `crates/tetris-app/src/modes.rs`, `crates/tetris-app/src/hud.rs`, `crates/tetris-app/src/screens_menu.rs`
- **description**: Catalogue entry (config: feed + no goal + marathon
  gravity). HUD: elapsed clock, the existing queued-garbage meter (reuse the
  versus pending-garbage display, fed from `pending_garbage()`), and time
  until the next row. Result on top-out = time survived, recorded as
  `BestTime` under Survival.
- **validation**: Headless test: bot survives ≥ 30 s of feed then eventually
  tops out with a recorded time; HUD fixture shows queue count and
  countdown-to-next-row strings.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T14: Zen mode (core wipe-on-block-out + app)
- **depends_on**: [T11, T5]
- **location**: `crates/tetris-core/src/game.rs` (`WipeAndContinue` branch in the spawn/block-out path), `crates/tetris-app/src/modes.rs`, `crates/tetris-app/src/hud.rs`, `crates/tetris-app/src/records.rs`
- **description**: Implement `BlockOutBehavior::WipeAndContinue`: on spawn
  collision, clear the board (whole stack — owner decision), re-spawn the
  colliding piece at its spawn state, and emit `GameEvent::StackWiped` (new
  variant, appended to the enum — wire-safe, events never cross the network).
  No `GameOver` ever fires; score/lines keep accumulating. App: catalogue
  entry (fixed level 1, no goal); HUD shows session lines + lifetime lines
  (`Records::LifetimeLines` — incremented on every line clear event,
  persisted via the debounced save path); exit via pause → menu.
- **validation**: Core test: force a block-out with `WipeAndContinue`, assert
  play continues, no `GameOver`, board empty, same-seed replay identical. App
  test: lifetime lines persist across a simulated restart of the config dir.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T15: Bot speed as a per-match parameter
- **depends_on**: [T11]
- **location**: `crates/tetris-app/src/core_bridge/versus.rs`
- **description**: Replace the `BOT_LOCK_COOLDOWN_STEPS` constant read inside
  `versus_bot_system` with a per-side cooldown stored on `VersusMatch`
  (`bot_cooldown_ticks: [u32; 2]`). **Non-breaking**: keep `start_versus`
  signature and behavior exactly as today (fills the default constant) and
  add `start_versus_with_cooldown(..., [u32; 2])` for the ladder (T16) — no
  call-site churn in `screens_menu.rs`/`harness.rs`. Pure plumbing: default
  behavior byte-identical (existing versus/harness/soak tests pass
  unmodified).
- **validation**: `cargo test --workspace` green with defaults; a unit test
  starts a bot-vs-bot match at 10-tick cooldown and asserts a materially
  higher lock rate than the 60-tick default.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T16: Bot Ladder campaign
- **depends_on**: [T15, T6, T7]
- **location**: `crates/tetris-app/src/screens_ladder.rs` (new), `crates/tetris-app/src/modes.rs`, `crates/tetris-app/src/records.rs`
- **description**: Ladder flow screen entered from mode select: 8 rungs,   `start_versus_with_cooldown(Garbage, Human, Bot, [c; 2])` per rung with
  cooldown constants
  `[120, 104, 82, 60, 44, 30, 19, 10]` (named const array, rung 4 == today's
  60; tuning lives here only). Win (via `VersusWinner`) unlocks the next rung;
  loss retries. Persist `HighestRung` in `Records`; locked rungs disabled in
  the list; versus HUD gains the rung number. Rematch/menu buttons reuse the
  T26 versus overlay flow.
- **validation**: Headless test: bot at 10-tick cooldown beats a 120-tick
  ladder seat (win path unlocks), winner resource drives unlock + record;
  rung persistence round-trip under `TETRIS_CONFIG_DIR`.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T17: Daily Challenge
- **depends_on**: [T6, T7]
- **location**: `crates/tetris-app/src/daily.rs` (new), `crates/tetris-app/src/modes.rs`, `crates/tetris-app/src/screens_menu.rs` (result line), `crates/tetris-app/src/records.rs`
- **description**: App-side (core never sees a date): today's UTC date
  (`SystemTime` → civil date helper) → seed via splitmix64; weekday maps the
  day's mode among Sprint/Ultra/Dig by a named `const DAILY_ROTATION`
  (weekday-index % 3). Mode-select row shows today's mode + "your result /
  not yet". First **completed** run of the day records
  `Daily { date, ticks-or-score }`; retries the same day update nothing
  (first-counts, owner decision). Result screen shows the share line
  `Blockfall Daily 2026-10-01 · Dig · 1:42.35` as display-only selectable
  text (no clipboard in Bevy 0.19 — owner decision fallback).
- **validation**: Unit tests: date→seed stable and distinct across dates;
  rotation mapping; first-run-wins recording (second run ignored); share
  line format.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T18: RELEASE 2 GATE
- **depends_on**: [T13, T14, T16, T17]
- **location**: repo-wide
- **description**: Full validation suite green (wire canary T1 still holds —
  R2 must not have touched snapshot serialization). Playtest Survival feed
  constants and ladder rung curve (PRD risk: trim to fewer rungs if uneven).
  Manual Android portrait check for Survival/Zen/Bot Ladder/Daily.
- **validation**: All green + playtest notes in log. No R3 task starts before
  this gate.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T19: Match-level tick clock + protocol bump + new AttackRule variants
- **depends_on**: [T18]
- **location**: `crates/tetris-core/src/versus.rs`, `crates/tetris-app/src/core_bridge/net/protocol.rs`, `crates/netplay-gateway` (only if it inspects `AttackRule`)
- **description**: The only wire-breaking task. `AttackRule` gains `Dig` and
  `Switch { swap_interval_ticks: u32, warning_ticks: u32 }` **appended**
  (existing variant indices unchanged). `Match` gains a match-level `ticks`
  counter + `MatchEvent::SwapWarning { at_tick }` /
  `MatchEvent::BoardSwapped { tick }`, all serialized into
  `MatchSnapshot` (new fields → new bytes — that's what the bump covers).
  `PROTOCOL_VERSION` "0.1.0" → "0.2.0" (handshake then refuses mixed builds —
  PRD says ship desktop + Android together, note in changelog). `snapshot_hash
  = FNV-1a over bincode(MatchSnapshot)` automatically covers the new fields;
  add a test asserting old-rule snapshots differ from new-struct snapshots so
  a stale field can't ride silently.
- **validation**: Protocol roundtrip tests for every new variant; version
  handshake rejection test for old↔new; `snapshot_hash` coverage test;
  gateway self-test still passes (`cargo run -p netplay-gateway --
  --self-test`).
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T20: Dig Duel rule (core + local 1v1 first)
- **depends_on**: [T19]
- **location**: `crates/tetris-core/src/versus.rs`, `crates/tetris-app/src/core_bridge/versus.rs`, `crates/tetris-app/src/screens_menu.rs` (rule buttons), `crates/tetris-app/src/hud.rs`
- **description**: Core: `AttackRule::Dig` — both sides start with the same
  10 buried garbage rows (same hole columns on both boards, derived from the
  match seed), same piece sequence (`Match` same-pieces option: both side
  game seeds identical), no garbage ever sent. Win: first side whose
  `garbage_rows_left()` hits 0; a top-out loses immediately (opponent wins).
  App: new rule button under 1 v 1 (Garbage / Race / Dig) driving
  `start_versus` locally first (no protocol exposure beyond T19's); versus
  HUD gains garbage-rows-left per side.
- **validation**: Core tests: identical boards+sequences from one seed;
  first-to-clear crowns winner; top-out hands win over; replay determinism.
  Local human-vs-bot Dig Duel plays through.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T21: Switch rule (core + local 1v1 first)
- **depends_on**: [T19, T20]
- **location**: `crates/tetris-core/src/versus.rs`, `crates/tetris-app/src/core_bridge/versus.rs`, `crates/tetris-app/src/screens_menu.rs`, `crates/tetris-app/src/hud.rs`
- **description**: Core: Garbage attacks as usual; every `swap_interval_ticks`
  (default 1 800) the two sides' **entire game states swap** (board, bag
  state, hold, active piece, per-game counters) together with their pending
  garbage (decision: follows the board). Swap executes at the tick boundary
  after both sides ticked, deterministically on both peers; `SwapWarning`
  emitted `warning_ticks` (default 180 = 3 s) before. Winner = opponent of
  the side that tops out (as Garbage). App: rule button + swap countdown in
  the versus HUD.
- **validation**: Core tests: swap happens at exactly the right tick; state
  equivalence after swap (left-after == right-before); warning lead time;
  pending garbage moves with the board; replay determinism; no double-swap on
  asymmetric action timing.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T22: Online exposure + soak + relay e2e for Dig Duel and Switch
- **depends_on**: [T20, T21]
- **location**: `crates/tetris-app/src/core_bridge/net/{protocol.rs,session.rs,gateway.rs,online_ui.rs,harness.rs}`, `crates/netplay-gateway/tests/*`
- **description**: Online UI: host picks the rule (Garbage/Race/Dig/Switch) —
  it travels on `MatchStart` already; guest mirrors (unknown variants are gone
  post-bump). Extend `soak_rule(i)` in `harness.rs` so the 20-match nightly
  soak (`-- --ignored`) covers each new rule, and add Dig/Switch cases to the
  relay loopback + NAT-punch e2e tests. Zero snapshot-hash mismatches is the
  PRD gate. Switch risk: consider a brief input freeze on swap ticks —
  playtest locally first, implement behind a named const if needed.
- **validation**: `cargo test -p tetris-app -- --ignored` soak green for every
  rule; relay e2e green; version-skew handshake test.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T23: Mutator framework + cheap mutators
- **depends_on**: [T18, T7]
- **location**: `crates/tetris-app/src/mutators.rs` (new), `crates/tetris-app/src/screens_modes.rs`, `crates/tetris-app/src/input.rs`, `crates/tetris-app/src/render.rs`, `crates/tetris-app/src/hud.rs`, `crates/tetris-app/src/records.rs`
- **description**: `Mutators` resource selected on the mode-select screen per
  run (toggles: Invisible, No Hold, No Ghost, One Preview, 20G). Wiring per
  PRD R7: **No Ghost** = render skips ghost cells; **One Preview** = HUD next
  queue forced to 1; **20G** = `ModeConfig.start_level = 20` (uses R1); **No
  Hold** = filter `Action::Hold` in the bridge before `PendingActions`
  reaches the core (core untouched); **Invisible** in T24. Mutated runs never
  write best records (owner decision) but still bump mode play counters —
  enforce centrally in `Records::record_run(mutated: bool)`.
- **validation**: Headless tests per mutator: hold press is a silent no-op
  with No Hold; `snapshot_with_next` untouched but HUD shows 1; ghost absent
  in drawn cells (render test pattern `cells_of(app, CellKind::...)` exists);
  20G config start level; record suppressed + play count incremented with any
  mutator active.
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T24: Invisible mutator (lock fade)
- **depends_on**: [T23]
- **location**: `crates/tetris-app/src/render.rs`
- **description**: Render-only: locked cells fade out over/after 1 s
  (60 fixed steps). Render layer tracks per-cell lock age: when the board
  snapshot grows a cell between frames, stamp `spawn_frame`; fade alpha from
  age. Sprite pool already in `render.rs` carries per-cell state. Must not
  touch the core or snapshot; active piece and ghost unaffected.
- **validation**: Render test: after a lock, the locked cells' sprite alpha
  matches the fade curve at sampled ages; disabled mutator ⇒ today's exact
  colors (regression test reuses `drawn()` helper).
- **status**: Not Completed
- **log**:
- **files edited/created**:

### T25: RELEASE 3 GATE + docs
- **depends_on**: [T22, T23, T24]
- **location**: repo-wide; `PRD.md` §14 item 3; `CHANGELOG.md`; check-in of `game-modes-PRD.md`
- **description**: Full validation incl. per-rule 20-match soaks and relay
  e2e; clippy/fmt; Android APK portrait check for Dig Duel, Switch and
  mutators (mode screens reachable with touch only; Switch swap warning
  visible). Docs: bump `PROTOCOL_VERSION` note in README/CHANGELOG ("desktop
  and Android must update together"), add `game-modes-PRD.md` beside
  `PRD.md` and close §14 item 3 (**pending the owner checkbox**), update the
  README modes table.
- **validation**: All green; changelog reviewed.
- **status**: Not Completed
- **log**:
- **files edited/created**:

## Parallel Execution Groups

Shared-file hubs (`modes.rs` catalogue pre-declared in T5 minimizes this,
`hud.rs`, `screens_menu.rs`, `records.rs`, `versus.rs` core+bridge): **at
most one task per wave may edit a given file; the coordinator serializes
colliding tasks** — parallelism is the default, same-file tasks queue.

| Wave | Tasks | Can Start When |
|------|-------|----------------|
| 1 | T1 | Immediately |
| 2 | T2, T6 | T1 done |
| 3 | T3, T5 | T2 done |
| 4 | T4, T7, T8 | T3+T2→T4; T5(+T6)→T7; T5→T8 |
| 5 | T9, T10 | T5, T6, T8→T9; T3, T4, T5→T10 |
| 6 | **T11 — R1 gate** | T7–T10 done |
| 7 | T12, T15 | R1 gate passed (T14 waits: same core `game.rs` as T12) |
| 8 | T13, T14, T17 | T12→T13; T11→T14; T6, T7→T17 |
| 9 | T16 | T15 (and ladder UI files free) |
| 10 | **T18 — R2 gate** | T13, T14, T16, T17 done |
| 11 | T19 | R2 gate passed |
| 12 | T20, T23 | T19→T20; T18→T23 (mutators touch `hud.rs` — serialize with T20's HUD edits) |
| 13 | T21, T24 | T20→T21; T23→T24 |
| 14 | T22 | T20, T21 done |
| 15 | **T25 — R3 gate + docs** | T22, T24 done |

## Testing Strategy

- **Regression gate everywhere**: T1 golden snapshot + bincode-canary test
  runs on every task; wire shape of `GameSnapshot`/`MatchSnapshot` may only
  change in T19.
- **Core**: unit tests per mode config in `mode.rs`/`game.rs`/`versus.rs`;
  same-seed replay test per new behavior (event streams + snapshots equal);
  `modes_headless.rs` bot-completion tests run on every CI push.
- **App**: headless `MinimalPlugins`/hidden-window tests following the
  existing patterns in `core_bridge/mod.rs`, `hud.rs` fixtures and
  `render.rs` drawn-cell probes.
- **Netplay**: protocol roundtrip + hostile-fuzz (existing proptest covers
  new variants once added to the strategy), 20-match nightly soak
  (`cargo test --workspace -- --ignored`) extended per rule, relay loopback +
  NAT-punch e2e.
- **Persistence**: every records test isolates `TETRIS_CONFIG_DIR` under the
  existing `ENV_LOCK`; migration tested from a literal legacy `best.json`.
- **Phone parity**: manual Android APK portrait check at each release gate
  (start / play / leave each shipped mode with touch only).
- **CI commands**: `cargo test --workspace`, `cargo clippy --workspace -- -D
  warnings`, `cargo fmt --all --check`.

## Risks & Mitigations

- **Reopening the frozen core contract** → done once in T2, default config =
  Marathon, hard-gated by T1 golden test at T11.
- **Wire format break lands too early** → bincode byte-canary assertion in T1
  fires in CI the moment anyone touches `GameSnapshot`/`MatchSnapshot` before
  T19; protocol bump isolated to T19.
- **Old/new builds can't interop post-R3** → version handshake (existing
  mechanism) + changelog: ship desktop and Android together.
- **Phone menu crowding** → scrolling list, one line per mode (T7); versus
  rules stay under 1 v 1 / Online, not in the solo list.
- **Ladder curve uneven** (greedy bot gets faster, not smarter) → speeds are
  a single named const array in T16, tunable without touching logic; ship
  fewer rungs if needed.
- **Switch disorients under input delay** → 3 s warning mandatory; optional
  swap-tick input freeze behind a const, decided after local playtest (T21)
  before online soak (T22).
- **Survival/Ultra/Switch timings are guesses** → all named constants,
  playtest tuning at the release gates (T11/T18).
- **`best.json` migration corrupts a real profile** → additive read of the
  legacy shape before ever writing the new one; atomic writes already in
  place; corruption falls back to defaults as today.
- **Zen lifetime-lines persistence churn** → piggyback on the existing
  debounced save, not per-line writes.
