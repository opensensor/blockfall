//! Fixed-timestep bridge between Bevy and `tetris-core::Game` (T10).
//!
//! Names other tasks import from this module:
//!
//! - [`GameCore`] — resource owning the deterministic `tetris_core::game::Game`
//!   (`pub` fields `game`, `seed`, `steps`; `restart()`/`restart_with(seed)`).
//!   T11 renders exclusively from `core_bridge.game.snapshot()`.
//! - [`PendingActions`] — `pub queue: VecDeque<tetris_core::actions::Action>`
//!   that T12 pushes into; drained by the fixed step below and *held* (not
//!   dropped) while [`SimPaused`] is set (T19 freeze frames).
//! - [`SimPaused`] — `pub` tuple `SimPaused(pub bool)`; T17 pause and T19
//!   freeze frames toggle it.
//! - [`CoreEvent`] — app-side message newtype over `GameEvent`. Consume core
//!   events with `MessageReader<CoreEvent>` (registered via
//!   `app.add_message::<CoreEvent>()`). Bevy 0.19.1 renamed the `Events<T>`
//!   collection to `Messages<M>` / `add_event()` to `add_message()`, and `M:
//!   Message` must be a crate-local type (`tetris-core` stays Bevy-free, so a
//!   newtype is the only orphan-rule-clean way to carry `GameEvent`).
//! - [`restart_run`] (T14) — shared R-restart path honoring the `TETRIS_SEED`
//!   env seed; the plugin also hosts the `TETRIS_BOT=1` greedy solver +
//!   marathon logger (`MARATHON fps_avg=…`, `BOT game_done …`).
//! - **T5 solo-mode bridge** — [`start_mode_run`] (mode-aware start honoring
//!   `TETRIS_SEED`, built on [`GameCore::start_mode`]; bumps
//!   `Records::bump_plays` for the mode), `GameCore::active_mode` (current
//!   `ModeId` + `ModeConfig`; `restart_on_r_system` retries this id), and
//!   [`Countdown`] (pre-roll budget gating stepping so core tick 0 == first
//!   playable frame). Terminal `GoalReached`/`TimeUp` events flip
//!   `AppState::GameOver`; the exact reason comes from
//!   `GameCore.game.finished_reason()`. Catalogue in `crate::modes`.
//! - **T25 versus submodule** (`versus.rs`, re-exported here): `VersusMatch`
//!   (NonSend — it owns two `Game`s), `VersusWinner`, `VersusEvent`
//!   (`Messages`), `VersusHarness`, `Controller`, `start_versus`/`end_versus`
//!   and the `TETRIS_1V1=garbage|race` bot-vs-bot harness. While
//!   `VersusMatch.active` is set, the solo step/bot systems above freeze and
//!   no `CoreEvent`s fire; the versus side of the input contract is
//!   `VersusActions` (produced in `input.rs`, drained by the versus step).
//!
//! Schedule placement: `core_bridge_system` runs in `FixedUpdate`, a
//! sub-schedule of `FixedMain` (`bevy::app::FixedMain`), which the Main
//! schedule runs inside `RunFixedMainLoop` — between `PreUpdate` and
//! `Update`, i.e. strictly ahead of the render schedules (`Update`/
//! `PostUpdate`). The bridge pins the fixed clock to 60 Hz by overwriting the
//! `Time<Fixed>` resource (`TimePlugin`'s default is 64 Hz).

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;

use tetris_core::actions::Action;
use tetris_core::board::{self, Board, COLS, ROWS};
use tetris_core::event::GameEvent;
use tetris_core::game::{Game, GameSnapshot};
use tetris_core::mode::{Goal, ModeConfig};
use tetris_core::piece::{PieceState, Rotation};

use crate::modes::{self, ModeId};
use crate::records::Records;
use crate::state::AppState;

mod versus;
pub use versus::*;

pub(crate) mod net;

/// Env var overriding the run seed with a fixed `u64` (T14: reproducible
/// marathons; applies at startup and on every restart).
pub const SEED_ENV: &str = "TETRIS_SEED";

/// Env var enabling the greedy snapshot bot + marathon logging (`"1"`).
pub const BOT_ENV: &str = "TETRIS_BOT";

/// Games the bot plays before exiting the app (M2 gate marathon).
const BOT_GAMES: u32 = 2;

/// Seconds the bot waits after a game over before restarting.
const BOT_RESTART_DELAY_SECS: f32 = 0.5;

/// Parsed [`SEED_ENV`] value, if set and a valid `u64`.
fn env_seed() -> Option<u64> {
    std::env::var(SEED_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// [`BOT_ENV`] set to exactly `"1"`.
fn bot_enabled() -> bool {
    std::env::var(BOT_ENV).is_ok_and(|v| v.trim() == "1")
}

/// Simulation rate of the fixed-step schedule, Hz (PRD: core ticks at 60 Hz).
pub const SIM_HZ: f64 = 60.0;

/// Seed source for a fresh game: nanoseconds since the Unix epoch.
fn wall_clock_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED_F00D)
}

/// Resource owning the deterministic ruleset run (T10).
///
/// **Stored as a Bevy *non-send* resource** (`Res<NonSend<GameCore>>`,
/// `NonSendMut<GameCore>`, `world.non_send::<GameCore>()`) because
/// `tetris_core::game::Game` owns a `RefCell` RNG state and is therefore
/// `!Sync`, while Bevy 0.19.1 requires `Resource: Component: Send + Sync`.
pub struct GameCore {
    /// The authoritative game facade; render/HUD read `game.snapshot()`.
    pub game: Game,
    /// Seed the current run was started from.
    pub seed: u64,
    /// Number of fixed steps actually applied to `game` (diagnostics/tests).
    pub steps: u64,
    /// Core events emitted by the last step, forwarded to
    /// `Messages<CoreEvent>` on **every** fixed step — even while the step
    /// gate is closed, so T19 can freeze stepping and still let events drain.
    pub pending_events: Vec<GameEvent>,
    /// The solo mode this run was started as (T5). Read by systems holding
    /// `NonSend<GameCore>` (no separate resource, no borrow conflicts);
    /// `restart_on_r_system` retries this id.
    pub active_mode: ActiveMode,
}

impl GameCore {
    /// Fresh run from `seed` — Marathon (the default [`ActiveMode`]).
    pub fn new(seed: u64) -> Self {
        Self {
            game: Game::new(seed),
            seed,
            steps: 0,
            pending_events: Vec::new(),
            active_mode: ActiveMode::default(),
        }
    }

    /// Start a brand-new run seeded from wall time (T17's Game Over → retry).
    pub fn restart(&mut self) {
        let seed = wall_clock_seed();
        self.restart_with(seed);
    }

    /// Deterministic variant of [`GameCore::restart`] for tests and replays.
    /// Always Marathon (the legacy path): [`Self::active_mode`] is reset to
    /// match, so the R retry after a title-screen restart stays consistent.
    pub fn restart_with(&mut self, seed: u64) {
        self.game = Game::new(seed);
        self.seed = seed;
        self.steps = 0;
        self.pending_events.clear();
        self.active_mode = ActiveMode::default();
    }

    /// Start a **mode-aware** run: `Game::with_config(seed, &mode_config(id))`
    /// (T5). Marathon's config is [`ModeConfig::default`], so this reproduces
    /// [`GameCore::restart_with`] bit-for-bit for that mode. Seed resolution
    /// (`TETRIS_SEED`) and the countdown/play-count bookkeeping live in the
    /// free [`start_mode_run`], mirroring [`restart_run`].
    pub fn start_mode(&mut self, seed: u64, id: ModeId) {
        self.game = Game::with_config(seed, &modes::mode_config(id));
        self.seed = seed;
        self.steps = 0;
        self.pending_events.clear();
        self.active_mode = ActiveMode {
            id,
            config: modes::mode_config(id),
        };
    }
}

/// The solo mode a [`GameCore`] run was started as: the catalogue
/// [`ModeId`] plus the [`ModeConfig`] its `Game` was built from (T8's HUD
/// reads the goal/clock off `config` without re-deriving it from the id).
///
/// Lives as a field on `GameCore`, not a standalone resource: it is only
/// meaningful together with the `!Send` game it was built for, and systems
/// that need it already hold `NonSend<GameCore>` (plan T5 design note).
/// `Default` is Marathon, matching [`GameCore::default`].
#[derive(Debug, Clone)]
pub struct ActiveMode {
    /// Catalogue id of the live run.
    pub id: ModeId,
    /// The config `game` was constructed from.
    pub config: ModeConfig,
}

impl Default for ActiveMode {
    fn default() -> Self {
        Self {
            id: ModeId::Marathon,
            config: ModeConfig::default(),
        }
    }
}

/// Solo-start pre-roll budget (T5): remaining fixed steps of 3-2-1 countdown
/// before the core takes its first tick. `0` = no pre-roll (start
/// immediately).
///
/// Own gate, deliberately **not** [`SimPaused`]: the pre-roll is consumed
/// only while `AppState::Playing && !SimPaused.0`, so pausing mid-countdown
/// freezes it and resuming continues — never cancels (plan T5). Actions
/// arriving during the countdown stay queued in [`PendingActions`] exactly
/// like a freeze frame. Core tick 0 == first playable frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Resource)]
pub struct Countdown(pub u32);

/// Shared mode-aware start path (T5): what T7's mode-select rows and the
/// current-mode R retry call. Mirrors [`restart_run`]: a valid [`SEED_ENV`]
/// seeds the run for reproducibility, otherwise a wall-clock seed. Records
/// the mode on `core.active_mode` (via `core.start_mode`), re-arms
/// [`Countdown`] to the mode's budget (so a Sprint retry always gets its
/// 3-2-1), and counts the start in [`Records::bump_plays`].
///
/// Versus campaigns (BotLadder/DigDuel/Switch) start through `start_versus`,
/// not here — no play bump on that path is expected.
pub fn start_mode_run(
    id: ModeId,
    core: &mut GameCore,
    countdown: &mut Countdown,
    app_state: &mut AppState,
    records: Option<&mut Records>,
) -> u64 {
    let seed = match env_seed() {
        Some(seed) => {
            info!("start_mode {id:?}: seed {seed} (from {SEED_ENV})");
            seed
        }
        None => {
            let seed = wall_clock_seed();
            info!("start_mode {id:?}: seed {seed} (wall clock)");
            seed
        }
    };
    core.start_mode(seed, id);
    countdown.0 = modes::pre_roll_ticks(id);
    if let Some(records) = records {
        records.bump_plays(modes::mode_key(id));
    }
    *app_state = AppState::Playing;
    seed
}

/// Drains the [`Countdown`] budget one step per `FixedUpdate` while the solo
/// run is live and unpaused (T5). Registered **before**
/// [`core_bridge_system`], so the frame the budget hits zero is the first
/// frame whose gate is open. `SimPaused` or a non-`Playing` state freezes
/// the remaining budget instead of cancelling it.
fn countdown_system(
    mut countdown: ResMut<Countdown>,
    app_state: Res<AppState>,
    paused: Res<SimPaused>,
) {
    if countdown.0 > 0 && *app_state == AppState::Playing && !paused.0 {
        countdown.0 -= 1;
    }
}

impl Default for GameCore {
    fn default() -> Self {
        Self::new(wall_clock_seed())
    }
}

/// Player actions waiting to be consumed by the next fixed step (T12 pushes,
/// the bridge drains). Held, never dropped, while stepping is gated.
#[derive(Debug, Default, Resource)]
pub struct PendingActions {
    /// FIFO action queue.
    pub queue: VecDeque<Action>,
}

impl PendingActions {
    /// Enqueue one action for the next fixed step.
    pub fn push(&mut self, action: Action) {
        self.queue.push_back(action);
    }
}

/// Gate resource: when `true` the core is not stepped, but queued actions
/// are **held** for later and [`GameCore::pending_events`] still drain into
/// `Messages<CoreEvent>` (T17 pause, T19 freeze frames).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Resource)]
pub struct SimPaused(pub bool);

/// App-side message wrapper over the core's `GameEvent` (see module docs for
/// why a newtype is required).
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct CoreEvent(pub GameEvent);

/// Shared restart path for the human R key and the bot (T14). Honors
/// [`SEED_ENV`]: a fixed env seed makes marathons reproducible; otherwise a
/// wall-clock seed is used, as before.
pub fn restart_run(core: &mut GameCore, app_state: &mut AppState) {
    match env_seed() {
        Some(seed) => {
            info!("restart: seed {seed} (from {SEED_ENV})");
            core.restart_with(seed);
        }
        None => {
            core.restart();
            info!("restart: seed {} (wall clock)", core.seed);
        }
    }
    *app_state = AppState::Playing;
}

/// Startup: override the wall-clock seed of the plugin-inserted `GameCore`
/// when [`SEED_ENV`] is set, so even game #1 of a marathon is reproducible.
fn seed_from_env_at_startup(mut core: NonSendMut<GameCore>) {
    match env_seed() {
        Some(seed) => {
            info!("startup: seed {seed} (from {SEED_ENV})");
            core.restart_with(seed);
        }
        None => info!("startup: seed {} (wall clock)", core.seed),
    }
}

/// R (hardcoded — `KeyBindings` in `input.rs` owns only the eight action
/// slots plus pause and has no restart slot) restarts from Game Over (T14).
/// Since T5 it retries the **current mode** (`core.active_mode.id`,
/// re-arming its pre-roll) rather than raw Marathon; a default world is
/// Marathon, so the legacy behavior is unchanged there.
fn restart_on_r_system(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    core: NonSendMut<GameCore>,
    app_state: ResMut<AppState>,
    countdown: ResMut<Countdown>,
    mut records: Option<ResMut<Records>>,
) {
    // `Option<Res<...>>`: MinimalPlugins headless worlds have no
    // `InputPlugin`, mirroring T12's resource guard.
    let Some(keys) = keys else { return };
    if *app_state == AppState::GameOver && keys.just_pressed(KeyCode::KeyR) {
        let core = core.into_inner();
        let id = core.active_mode.id;
        start_mode_run(
            id,
            core,
            countdown.into_inner(),
            app_state.into_inner(),
            records.as_deref_mut(),
        );
    }
}

/// Bot toggle resource, from [`BOT_ENV`] at plugin build (T14).
#[derive(Resource)]
struct BotMode(bool);

/// Marathon bookkeeping for the bot: completed games, the post-game-over
/// restart countdown (plain f32 seconds; no `Timer` resource dance needed),
/// and the committed placement the step-wise executor is realizing.
#[derive(Resource, Default)]
struct BotState {
    games_done: u32,
    awaiting_restart: bool,
    restart_in: f32,
    plan: Option<BotPlan>,
    last_rot: Option<Rotation>,
    last_col: Option<i32>,
    wedge: u32,
}

/// A committed target for the currently active piece; re-planned whenever a
/// new piece spawns, so kicks/mispositions self-correct next step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BotPlan {
    piece: tetris_core::piece::Piece,
    rot: Rotation,
    target_col: i32,
}

/// Steps a committed piece toward its plan (one action per tick): rotate →
/// shift → hard drop. A stall counter forces the drop if rotation/moves stop
/// making progress (blocked kicks, wall pinning), so the bot can never hang.
/// Actions go through `push`, so solo (`PendingActions`) and versus (per-side
/// `VersusActions`, see [`versus`]) share one executor.
fn step_bot_plan(
    push: &mut dyn FnMut(Action),
    state: &mut BotState,
    active: PieceState,
    mv: BotPlan,
) {
    let progressed = state.last_rot != Some(active.rot) || state.last_col != Some(active.col);
    state.wedge = if progressed { 0 } else { state.wedge + 1 };
    state.last_rot = Some(active.rot);
    state.last_col = Some(active.col);
    if state.wedge > 8 {
        push(Action::HardDrop);
        state.plan = None;
        return;
    }
    if state.wedge >= 2 {
        // Plan went stale under the piece (blocked slide / gravity): re-solve
        // from the current position next tick instead of grinding into it.
        state.plan = None;
        return;
    }
    if active.rot != mv.rot {
        let diff = (mv.rot as u32 + 4 - active.rot as u32) % 4;
        push(match diff {
            1 => Action::RotateCw,
            3 => Action::RotateCcw,
            _ => Action::Rotate180,
        });
    } else if active.col != mv.target_col {
        push(if active.col < mv.target_col {
            Action::MoveRight
        } else {
            Action::MoveLeft
        });
    } else {
        push(Action::HardDrop);
        state.plan = None;
    }
}

/// One bot brain tick for a snapshot: execute the committed plan or commit a
/// fresh greedy one for a new piece. Shared by the solo marathon driver and
/// the versus per-side bot driver (T25).
fn bot_side_drive(snapshot: &GameSnapshot, state: &mut BotState, push: &mut dyn FnMut(Action)) {
    let Some(active) = snapshot.active else {
        state.plan = None;
        return;
    };
    match state.plan {
        Some(mv) if mv.piece == active.piece => {
            step_bot_plan(push, state, active, mv);
        }
        _ => {
            state.plan = bot_move(snapshot).map(|mv| BotPlan {
                piece: active.piece,
                rot: mv.rot,
                target_col: mv.target_col,
            });
            state.wedge = 0;
            state.last_rot = Some(active.rot);
            state.last_col = Some(active.col);
        }
    }
}

/// Bot brain: every fixed step (before the bridge drains) execute the
/// committed plan or commit a fresh greedy one for a new piece. Runs in
/// `FixedUpdate` *before* `core_bridge_system`, so actions apply same-tick.
/// Asleep while a versus match is active (and during a T5 pre-roll — its
/// actions would merely queue against a frozen core).
// System params: every input is needed; an exclusive-system wrapper would
// obscure the resource types.
#[allow(clippy::too_many_arguments)]
fn bot_drive_system(
    bot: Res<BotMode>,
    mut pending: ResMut<PendingActions>,
    state: ResMut<BotState>,
    core: NonSend<GameCore>,
    app_state: Res<AppState>,
    paused: Res<SimPaused>,
    countdown: Res<Countdown>,
    versus: NonSend<VersusMatch>,
) {
    if !bot.0 || *app_state != AppState::Playing || paused.0 || countdown.0 > 0 || versus.active {
        return;
    }
    let snapshot = core.game.snapshot();
    bot_side_drive(&snapshot, state.into_inner(), &mut |a| pending.push(a));
}

/// Frame-time accumulator for `MARATHON fps_avg=...` logs (bot mode only).
#[derive(Resource, Default)]
struct MarathonStats {
    frames: u64,
    frame_secs: f64,
}

/// Placement metrics for the greedy solver over a board of settled cells
/// (row 0 = top, hidden rows included): `(aggregate column height, holes,
/// bumpiness)`. Call on the post-clear board.
fn stack_metrics(board: &Board) -> (i32, i32, i32) {
    let mut agg = 0;
    let mut holes = 0;
    let mut heights = [0i32; COLS];
    for (col, height) in heights.iter_mut().enumerate() {
        let mut filled_seen = false;
        for row in 0..ROWS {
            if board.get(row, col).is_some() {
                if !filled_seen {
                    filled_seen = true;
                    *height = (ROWS - row) as i32;
                }
            } else if filled_seen {
                holes += 1;
            }
        }
        agg += *height;
    }
    let bump = heights.windows(2).map(|w| (w[0] - w[1]).abs()).sum();
    (agg, holes, bump)
}

/// A chosen placement: rotate `active` to `rot`, shift to `target_col`, hard
/// drop. SRS kicks are ignored (deliberately greedy; not a perfect solver).
#[derive(Clone, Copy, Debug)]
struct BotMove {
    rot: Rotation,
    target_col: i32,
}

/// All resting placements `(row, col)` of `base` (one fixed rotation) that
/// the piece can actually reach by sliding and falling — BFS over the
/// non-colliding `(row, col)` states from the spawn position, collecting the
/// states where the piece rests (ghost == self). Ignores in-flight rotation
/// and kicks; a superset of free-fall, exact for slide+drop.
fn reachable_placements(board: &Board, base: PieceState) -> Vec<(i32, i32)> {
    let mut seen = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    seen.insert((base.row, base.col));
    queue.push_back(base);
    let mut out = Vec::new();
    while let Some(s) = queue.pop_front() {
        if board::ghost_row(board, &s) == s.row {
            out.push((s.row, s.col));
        }
        for next in [
            PieceState {
                row: s.row + 1,
                ..s
            },
            PieceState {
                col: s.col - 1,
                ..s
            },
            PieceState {
                col: s.col + 1,
                ..s
            },
        ] {
            if !board.collides(&next) && seen.insert((next.row, next.col)) {
                queue.push_back(next);
            }
        }
    }
    out
}

/// Greedy snapshot solver: every rotation × every *reachable* resting slot
/// (slide+drop BFS), simulated via `merge` + clear. Dellacherie-flavoured
/// weights: clears dominate, then holes, aggregate height, bumpiness. `None`
/// when no placement fits (block-out imminent).
fn bot_move(snapshot: &GameSnapshot) -> Option<BotMove> {
    let active = snapshot.active?;
    let mut best: Option<((i32, i32), BotMove)> = None;
    for rot in [Rotation::Spawn, Rotation::Cw, Rotation::R180, Rotation::Ccw] {
        let base = PieceState {
            piece: active.piece,
            rot,
            row: active.row,
            col: active.col,
        };
        if snapshot.board.collides(&base) {
            continue;
        }
        for (row, col) in reachable_placements(&snapshot.board, base) {
            let placed = PieceState { row, col, ..base };
            let mut sim = snapshot.board.clone();
            sim.merge(&placed);
            let cleared = sim.full_rows().len() as i32;
            sim.clear_full_rows();
            let (agg, holes, bump) = stack_metrics(&sim);
            let score = cleared * 4500 - holes * 500 - agg * 25 - bump * 12;
            let key = (score, -(col - active.col).abs());
            if best.is_none_or(|(best_key, _)| key > best_key) {
                best = Some((
                    key,
                    BotMove {
                        rot,
                        target_col: col,
                    },
                ));
            }
        }
    }
    best.map(|(_, mv)| mv)
}

/// Bot lifecycle (real frames): on Game Over log per-game stats and
/// `MARATHON game_done`, restart via the shared R path after a short delay,
/// and exit with `AppExit::Success` once [`BOT_GAMES`] games completed.
fn bot_marathon_system(
    bot: Res<BotMode>,
    mut bot_state: ResMut<BotState>,
    mut core: NonSendMut<GameCore>,
    mut app_state: ResMut<AppState>,
    time: Res<Time>,
    versus: NonSend<VersusMatch>,
    mut exits: MessageWriter<AppExit>,
) {
    if !bot.0 || versus.active {
        return;
    }
    if *app_state == AppState::Title && !bot_state.awaiting_restart {
        // T17 moved the app to Title at startup; QA bot mode presses "Start"
        // the same way R restarts.
        info!("BOT start from Title (Start-equivalent)");
        restart_run(&mut core, &mut app_state);
    }
    if *app_state == AppState::GameOver && !bot_state.awaiting_restart {
        bot_state.games_done += 1;
        let snapshot = core.game.snapshot();
        info!(
            "BOT game_done seed={} score={} level={} lines={}",
            core.seed, snapshot.score, snapshot.level, snapshot.lines
        );
        info!("MARATHON game_done games_done={}", bot_state.games_done);
        if bot_state.games_done >= BOT_GAMES {
            exits.write(AppExit::Success);
            return;
        }
        bot_state.awaiting_restart = true;
        bot_state.restart_in = BOT_RESTART_DELAY_SECS;
    }
    if bot_state.awaiting_restart {
        bot_state.restart_in -= time.delta_secs();
        if bot_state.restart_in <= 0.0 {
            info!("BOT restart (R-equivalent)");
            restart_run(&mut core, &mut app_state);
            bot_state.awaiting_restart = false;
        }
    }
}

/// Real-window frame-time reporter: every ~2 s logs
/// `MARATHON fps_avg=<x> frame_ms=<y>` from actual frame deltas (bot mode
/// only, to keep normal runs quiet).
fn marathon_fps_system(bot: Res<BotMode>, mut stats: ResMut<MarathonStats>, time: Res<Time>) {
    if !bot.0 {
        return;
    }
    stats.frames += 1;
    stats.frame_secs += time.delta_secs() as f64;
    if stats.frame_secs >= 2.0 {
        let fps = stats.frames as f64 / stats.frame_secs;
        let frame_ms = stats.frame_secs * 1000.0 / stats.frames as f64;
        info!("MARATHON fps_avg={fps:.1} frame_ms={frame_ms:.2}");
        stats.frames = 0;
        stats.frame_secs = 0.0;
    }
}

/// Steps the core on the fixed schedule. Registered in `FixedUpdate` (a
/// `FixedMain` sub-schedule run ahead of `Update`/render — never in a render
/// or `Update` schedule). Fully frozen while a versus match is active, so
/// the solo `CoreEvent` stream and `AppState` can never react to versus
/// play (T25). Also frozen while [`Countdown`] still has budget (T5
/// pre-roll): queued actions are **held** in [`PendingActions`] exactly like
/// a [`SimPaused`] freeze, and core tick 0 is the first playable frame.
/// Terminal `GameEvent::GoalReached`/`TimeUp` (T2) flip `AppState::GameOver`
/// just like `GameOver` does — the events are forwarded either way, and T9
/// reads the exact reason from `Game::finished_reason()`.
fn core_bridge_system(
    core: NonSendMut<GameCore>,
    mut pending: ResMut<PendingActions>,
    mut messages: MessageWriter<CoreEvent>,
    mut app_state: ResMut<AppState>,
    paused: Res<SimPaused>,
    countdown: Res<Countdown>,
    versus: NonSend<VersusMatch>,
) {
    let core = core.into_inner();
    if !paused.0 && countdown.0 == 0 && *app_state == AppState::Playing && !versus.active {
        for action in pending.queue.drain(..) {
            core.pending_events.extend(core.game.apply(action));
        }
        core.pending_events.extend(core.game.tick());
        core.steps += 1;
    }

    let mut terminal = false;
    for event in core.pending_events.drain(..) {
        if matches!(
            event,
            GameEvent::GameOver | GameEvent::GoalReached { .. } | GameEvent::TimeUp { .. }
        ) {
            terminal = true;
        }
        messages.write(CoreEvent(event));
    }
    if terminal && *app_state == AppState::Playing {
        *app_state = AppState::GameOver;
    }
}

/// Per-frame HUD feed for the mode-aware widgets (T8). The bridge writes it
/// every fixed step **after** [`core_bridge_system`] (see
/// [`mode_hud_refresh_system`]); HUD systems only read it. `GameSnapshot`
/// gains **no** fields — this resource is the wire-stable carrier for
/// clock/goal data (plan constraint 1).
///
/// Field notes for later tasks:
/// - `clock_ticks`: `Game::tick_count()` — 0 during a pre-roll (core frozen),
///   first playable frame is 1.
/// - `clock_limit`: `ModeConfig::clock_ticks` (Ultra 7 200; `None` counts up).
/// - `show_hud`: the mode has a goal *or* a clock — Marathon stays `false`
///   and renders neither clock nor goal row.
/// - `feed_pending` / `feed_next_row_in`: reserved for T13 (Survival garbage
///   feed, filled by T12); `swap_in`: reserved for T21 (Switch swap timer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Resource)]
pub struct ModeHudInfo {
    /// The mode this feed describes (catalogue id of `GameCore::active_mode`).
    pub mode_id: ModeId,
    /// Elapsed core ticks (`Game::tick_count()`).
    pub clock_ticks: u64,
    /// Count-down budget when the mode has one (`ModeConfig::clock_ticks`).
    pub clock_limit: Option<u64>,
    /// Remaining pre-roll steps ([`Countdown`] mirror); `0` once playable.
    pub countdown: u32,
    /// Sprint: `goal - lines` remaining; `None` without a lines goal.
    pub lines_left: Option<u32>,
    /// Dig: rows of garbage left (`Game::garbage_rows_left()`); `None`
    /// without the garbage goal.
    pub garbage_left: Option<usize>,
    /// Pieces locked since this mode's start (from `PieceLocked` events).
    pub pieces_placed: u32,
    /// `true` when the mode asks for a clock/goal row.
    pub show_hud: bool,
    /// Reserved (T13): queued garbage rows pending on this board.
    pub feed_pending: Option<u32>,
    /// Reserved (T13): ticks until the next feed row lands.
    pub feed_next_row_in: Option<u64>,
    /// Reserved (T21): ticks until the next whole-game swap.
    pub swap_in: Option<u64>,
}

impl Default for ModeHudInfo {
    fn default() -> Self {
        Self {
            mode_id: ModeId::Marathon,
            clock_ticks: 0,
            clock_limit: None,
            countdown: 0,
            lines_left: None,
            garbage_left: None,
            pieces_placed: 0,
            show_hud: false,
            feed_pending: None,
            feed_next_row_in: None,
            swap_in: None,
        }
    }
}

/// Refresh [`ModeHudInfo`] from the core getters every fixed step. Runs
/// **after** [`core_bridge_system`] in `FixedUpdate` — same slot discipline
/// as the bridge itself, so the very `Update` render pass that follows reads
/// the post-step values.
///
/// `pieces_placed` reset: watches `GameCore::steps == 0`. Both `restart_with`
/// and `start_mode` zero `steps` *before* the next step, while this system's
/// same-step position behind the bridge means the first playable frame
/// already reports `steps == 1` — so the fresh-run sentinel is unambiguous
/// (pre-roll frames re-report 0, which is correct: nothing has locked yet).
fn mode_hud_refresh_system(
    core: NonSend<GameCore>,
    countdown: Res<Countdown>,
    mut events: MessageReader<CoreEvent>,
    mut hud: ResMut<ModeHudInfo>,
) {
    let locks = events
        .read()
        .filter(|CoreEvent(event)| matches!(event, GameEvent::PieceLocked { .. }))
        .count() as u32;
    hud.pieces_placed = if core.steps == 0 {
        locks
    } else {
        hud.pieces_placed + locks
    };
    let config = &core.active_mode.config;
    hud.mode_id = core.active_mode.id;
    hud.clock_ticks = core.game.tick_count();
    hud.clock_limit = config.clock_ticks;
    hud.countdown = countdown.0;
    hud.lines_left = match config.goal {
        Some(Goal::Lines(target)) => Some(target.saturating_sub(core.game.snapshot().lines)),
        _ => None,
    };
    hud.garbage_left =
        matches!(config.goal, Some(Goal::GarbageCleared)).then(|| core.game.garbage_rows_left());
    hud.show_hud = config.goal.is_some() || config.clock_ticks.is_some();
    // Reserved fields (T13 survival feed / T21 swap timer) stay `None` until
    // their tasks fill them from the feed/swap state.
}

/// Spawns the primary 2D camera if no other plugin stub has one yet (render
/// logic itself is T11's; this keeps the shipped app displayable).
fn spawn_primary_camera(mut commands: Commands, cameras: Query<&Camera2d>) {
    if cameras.is_empty() {
        commands.spawn(Camera2d);
    }
}

/// Steps the deterministic core on the fixed-step schedule and drains its
/// events into Bevy `Messages<CoreEvent>`. Also mounts the T25 versus
/// bridge (`VersusBridgePlugin` — see the `versus` submodule); main.rs stays
/// frozen, so versus reaches the app exclusively through here.
pub struct CoreBridgePlugin;

impl Plugin for CoreBridgePlugin {
    fn build(&self, app: &mut App) {
        let bot = bot_enabled();
        info!("core bridge: bot mode {}", if bot { "ON" } else { "off" });
        app.insert_non_send(GameCore::default())
            .init_resource::<PendingActions>()
            .init_resource::<SimPaused>()
            // T5: the solo-start pre-roll budget (defaults to 0 — legacy
            // behavior; the mode id itself rides on `GameCore::active_mode`).
            .init_resource::<Countdown>()
            // T8: the bridge-written HUD feed (clock/goal/pre-roll data;
            // `GameSnapshot` must not grow — wire stability rule).
            .init_resource::<ModeHudInfo>()
            .add_message::<CoreEvent>()
            // Overwrite TimePlugin's default 64 Hz clock: the core contract
            // is 60 Hz (gravity, lock delay, DAS/ARR tick conversions all
            // assume it).
            .insert_resource(Time::<Fixed>::from_hz(SIM_HZ))
            // T14: env seed override, human R-restart, bot marathon.
            .insert_resource(BotMode(bot))
            .init_resource::<BotState>()
            .init_resource::<MarathonStats>()
            // T25: 1v1 versus bridge (inactive until a match starts).
            .add_plugins(VersusBridgePlugin)
            // N2: netplay session (netplay-plan.md) — same mount slot; the
            // bevy_renet plugins inside are resource-gated, so an Idle
            // session costs nothing per frame.
            .add_plugins(net::NetPlugin)
            // N6: netplay harness — the `TETRIS_NET=host:<port>|join:<addr>`
            // desktop bot-vs-bot-across-the-wire mode and the
            // `TETRIS_NET_FORK=<role>:<tick>` desync-injection hook, wired
            // exactly like the TETRIS_1V1 pattern above: all three systems
            // are inert no-ops (a couple of env reads) unless those vars are
            // set, so headless tests and normal play pay nothing.
            .add_systems(Startup, net::harness::net_harness_startup)
            .add_systems(Update, net::harness::net_harness_update)
            .add_systems(FixedUpdate, net::harness::net_fork_system)
            .add_systems(Startup, seed_from_env_at_startup)
            .add_systems(
                Update,
                (
                    restart_on_r_system,
                    bot_marathon_system,
                    marathon_fps_system,
                ),
            )
            .add_systems(Startup, spawn_primary_camera)
            .add_systems(FixedUpdate, core_bridge_system)
            .add_systems(
                FixedUpdate,
                (countdown_system, bot_drive_system).before(core_bridge_system),
            )
            // T8: HUD feed refreshed right after the step (never before),
            // so Update always renders post-step values.
            .add_systems(
                FixedUpdate,
                mode_hud_refresh_system.after(core_bridge_system),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::{FixedMain, FixedMainScheduleOrder, MainScheduleOrder, RunFixedMainLoop};
    use std::time::Duration;

    use bevy::ecs::schedule::ScheduleLabel;
    use tetris_core::game::NEXT_PREVIEW;

    /// Headless app (T1-style `MinimalPlugins` smoke pattern) with the
    /// bridge plus the T1-owned `AppState` resource `main.rs` provides.
    fn test_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(CoreBridgePlugin);
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app
    }

    /// Drive exactly one fixed step through the real `FixedUpdate`
    /// sub-schedule (deterministic; no wall-clock dependence).
    fn fixed_step(app: &mut App) {
        app.world_mut().run_schedule(FixedUpdate);
    }

    fn snapshot(app: &App) -> tetris_core::game::GameSnapshot {
        app.world().non_send::<GameCore>().game.snapshot()
    }

    fn drained(app: &mut App) -> Vec<CoreEvent> {
        app.world_mut()
            .resource_mut::<Messages<CoreEvent>>()
            .drain()
            .collect()
    }

    #[test]
    fn scripted_pending_actions_advance_the_core() {
        let mut app = test_app(0xC0FFEE);
        let before = snapshot(&app);

        app.world_mut()
            .resource_mut::<PendingActions>()
            .push(Action::HardDrop);
        fixed_step(&mut app);

        let after = snapshot(&app);
        assert_eq!(app.world().non_send::<GameCore>().steps, 1);
        assert!(after.score > before.score, "hard drop must score");
        assert!(after.active.is_some(), "next piece spawns after the lock");
        assert_ne!(after.board, before.board, "locked piece lands on board");

        let events = drained(&mut app);
        assert!(
            events
                .iter()
                .any(|e| matches!(e.0, GameEvent::PieceLocked { .. })),
            "lock event reaches Messages<CoreEvent>: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.0, GameEvent::ScoreChanged { .. })),
            "score event reaches Messages<CoreEvent>: {events:?}"
        );
        assert!(app.world().resource::<PendingActions>().queue.is_empty());
    }

    #[test]
    fn restart_resets_core_and_fresh_snapshot_is_fully_populated() {
        let mut app = test_app(1);
        for _ in 0..6 {
            app.world_mut()
                .resource_mut::<PendingActions>()
                .push(Action::HardDrop);
            fixed_step(&mut app);
        }
        assert!(snapshot(&app).score > 0, "precondition: game progressed");

        app.world_mut().non_send_mut::<GameCore>().restart_with(7);

        let after = snapshot(&app);
        assert_eq!(after, Game::new(7).snapshot());
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
        assert!(after.active.is_some(), "active piece spawns at tick 0");
        assert!(after.ghost_row.is_some());
        assert_eq!(after.next.len(), NEXT_PREVIEW, "next queue full at tick 0");
        assert_eq!(after.score, 0);
        assert!(!after.game_over);
    }

    #[test]
    fn core_is_not_stepped_outside_playing_state() {
        let mut app = test_app(2);
        *app.world_mut().resource_mut::<AppState>() = AppState::Title;
        app.world_mut()
            .resource_mut::<PendingActions>()
            .push(Action::HardDrop);

        for _ in 0..3 {
            fixed_step(&mut app);
        }

        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
        assert_eq!(snapshot(&app), Game::new(2).snapshot());
        assert_eq!(
            app.world().resource::<PendingActions>().queue.len(),
            1,
            "actions wait instead of being dropped"
        );
    }

    #[test]
    fn sim_paused_freezes_stepping_but_events_still_drain() {
        let mut app = test_app(3);
        app.world_mut().resource_mut::<SimPaused>().0 = true;
        app.world_mut()
            .resource_mut::<PendingActions>()
            .push(Action::HardDrop);
        // Event produced just before a T19-style freeze:
        app.world_mut()
            .non_send_mut::<GameCore>()
            .pending_events
            .push(GameEvent::PerfectClear);

        fixed_step(&mut app);

        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
        assert_eq!(snapshot(&app), Game::new(3).snapshot(), "core frozen");
        assert_eq!(
            app.world().resource::<PendingActions>().queue.len(),
            1,
            "queue HOLDs through the freeze (T19)"
        );
        assert_eq!(
            drained(&mut app),
            vec![CoreEvent(GameEvent::PerfectClear)],
            "events drain even while stepping is frozen"
        );
    }

    /// Behavior + structure proof that `core_bridge_system` lives on the
    /// fixed schedule (FixedMain → FixedUpdate) ahead of render/Update:
    /// running `Update` alone never steps the core, running `FixedUpdate`
    /// does, and `RunFixedMainLoop` precedes `Update` in the Main order.
    #[test]
    fn bridge_runs_in_fixed_schedule_not_in_update() {
        let mut app = test_app(4);

        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);

        fixed_step(&mut app);
        assert_eq!(app.world().non_send::<GameCore>().steps, 1);

        let order = app.world().resource::<MainScheduleOrder>();
        let labels = order.labels.to_vec();
        let fixed_pos = labels
            .iter()
            .position(|l| *l == RunFixedMainLoop.intern())
            .expect("RunFixedMainLoop scheduled");
        let update_pos = labels
            .iter()
            .position(|l| *l == Update.intern())
            .expect("Update scheduled");
        assert!(
            fixed_pos < update_pos,
            "fixed steps must run ahead of Update/render"
        );
        let fixed_order = app.world().resource::<FixedMainScheduleOrder>();
        assert!(
            fixed_order
                .labels
                .iter()
                .any(|l| *l == FixedUpdate.intern()),
            "FixedUpdate is part of FixedMain"
        );
        let _ = FixedMain; // names the umbrella schedule for readers
    }

    #[test]
    fn fixed_clock_is_pinned_to_60_hz() {
        let app = test_app(5);
        let timestep = app.world().resource::<Time<Fixed>>().timestep();
        assert!((timestep.as_secs_f64() - 1.0 / SIM_HZ).abs() < 1e-9);
        assert_eq!(
            app.world().resource::<Time<Fixed>>().timestep(),
            Duration::from_secs_f64(1.0 / SIM_HZ)
        );
    }

    #[test]
    fn core_game_over_flips_app_state() {
        let mut app = test_app(9);
        for _ in 0..1000 {
            app.world_mut()
                .resource_mut::<PendingActions>()
                .push(Action::HardDrop);
            fixed_step(&mut app);
            if *app.world().resource::<AppState>() == AppState::GameOver {
                break;
            }
        }
        assert_eq!(*app.world().resource::<AppState>(), AppState::GameOver);
        let events = drained(&mut app);
        assert!(events.iter().any(|e| e.0 == GameEvent::GameOver));
        assert!(snapshot(&app).game_over);
    }

    #[test]
    fn plugin_spawns_primary_camera_and_survives_real_frames() {
        let mut app = test_app(6);
        for _ in 0..3 {
            app.update();
        }
        let mut cameras = app.world_mut().query::<&Camera2d>();
        assert_eq!(cameras.iter(app.world()).count(), 1);
        // Resources are inserted by *this* plugin, so T1's stub-tree smoke
        // tests keep passing without main.rs knowing about them.
        assert!(app.world().contains_non_send::<GameCore>());
        assert!(app.world().contains_resource::<PendingActions>());
        assert!(app.world().contains_resource::<SimPaused>());
    }

    /// T14 M2-gate integration test: the *wired* bridge (CoreBridge only, no
    /// window) driven by the greedy solver from a fixed seed must complete
    /// the full loop — spawn → ≥1 LineCleared → LevelUp → GameOver — with
    /// zero panics, and the shared restart path (what the R key calls) must
    /// yield a fresh snapshot.
    #[test]
    fn wired_bot_bridge_full_loop_then_fresh_restart() {
        let mut app = test_app(42);
        *app.world_mut().resource_mut::<BotMode>() = BotMode(true);

        // Phase 1: solver plays through the real FixedUpdate wiring until it
        // has spawned, cleared lines and leveled up.
        let (mut saw_spawn, mut saw_clear, mut saw_levelup) = (false, false, false);
        for step in 0..3000 {
            fixed_step(&mut app);
            for event in drained(&mut app) {
                match event.0 {
                    GameEvent::PieceSpawned { .. } => saw_spawn = true,
                    GameEvent::LineCleared { .. } => saw_clear = true,
                    GameEvent::LevelUp { .. } => saw_levelup = true,
                    _ => {}
                }
            }
            if saw_spawn && saw_clear && saw_levelup {
                break;
            }
            assert_ne!(
                *app.world().resource::<AppState>(),
                AppState::GameOver,
                "solver died before level-up at fixed step {step}"
            );
        }
        assert!(
            saw_spawn && saw_clear && saw_levelup,
            "spawn={saw_spawn} line_clear={saw_clear} level_up={saw_levelup}"
        );

        // Phase 2: dumb hard drops (bot off) pile pieces at spawn until the
        // deterministic block-out, exercising the GameOver wiring.
        *app.world_mut().resource_mut::<BotMode>() = BotMode(false);
        let mut saw_game_over = false;
        for _ in 0..1000 {
            app.world_mut()
                .resource_mut::<PendingActions>()
                .push(Action::HardDrop);
            fixed_step(&mut app);
            let events = drained(&mut app);
            if events.iter().any(|e| e.0 == GameEvent::GameOver) {
                saw_game_over = true;
                break;
            }
        }
        assert!(saw_game_over, "hard-drop pile-up must block out");
        assert_eq!(*app.world().resource::<AppState>(), AppState::GameOver);
        assert!(snapshot(&app).game_over);

        // "R" restart through the shared glue (`restart_on_r_system` calls
        // exactly this once `AppState::GameOver` and KeyR are seen).
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, mut state| {
                let mut core = world.non_send_mut::<GameCore>();
                restart_run(core.as_mut(), state.as_mut());
            });

        assert_eq!(*app.world().resource::<AppState>(), AppState::Playing);
        let after = snapshot(&app);
        // No `TETRIS_SEED` in the test process, so the shared path restarts
        // from wall clock: assert structural freshness, not seed 42.
        assert!(after.active.is_some() && after.ghost_row.is_some());
        assert!(after.board.is_empty());
        assert_eq!(after.score, 0);
        assert_eq!(after.lines, 0);
        assert!(!after.game_over);
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
    }

    // ---- T5: mode-aware starts, pre-roll countdown, terminal reasons ----

    use crate::modes::{self, ModeId};
    use crate::records::Records;
    use tetris_core::mode::FinishReason;

    /// Start a mode through the shared T5 entry point, mirroring what the
    /// mode-select screen (T7) will call from a system.
    fn start_mode(app: &mut App, id: ModeId) {
        let world = app.world_mut();
        let mut state = world.remove_resource::<AppState>().unwrap();
        let mut countdown = world.remove_resource::<Countdown>().unwrap();
        let mut records = world.remove_resource::<Records>();
        {
            let mut core = world.non_send_mut::<GameCore>();
            start_mode_run(
                id,
                core.as_mut(),
                &mut countdown,
                &mut state,
                records.as_mut(),
            );
        }
        world.insert_resource(state);
        world.insert_resource(countdown);
        if let Some(records) = records {
            world.insert_resource(records);
        }
    }

    fn countdown(app: &App) -> u32 {
        app.world().resource::<Countdown>().0
    }

    fn steps(app: &App) -> u64 {
        app.world().non_send::<GameCore>().steps
    }

    #[test]
    fn sprint_start_holds_180_countdown_steps_before_first_core_tick() {
        let mut app = test_app(0x51);
        start_mode(&mut app, ModeId::Sprint);
        assert_eq!(countdown(&app), 180, "Sprint pre-roll budget");
        assert_eq!(
            app.world().non_send::<GameCore>().active_mode.id,
            ModeId::Sprint
        );

        app.world_mut()
            .resource_mut::<PendingActions>()
            .push(Action::HardDrop);
        // The pre-roll system runs before the step gate: 179 frames burn
        // budget with the core frozen, and the frame whose decrement
        // exhausts the budget (the 180th countdown step) is the first
        // playable frame — core tick 0 there.
        for _ in 0..179 {
            fixed_step(&mut app);
            assert_eq!(steps(&app), 0, "core frozen while pre-roll runs");
            assert_eq!(
                app.world().resource::<PendingActions>().queue.len(),
                1,
                "actions are held during the pre-roll, never dropped"
            );
        }
        assert_eq!(countdown(&app), 1);
        let run_seed = app.world().non_send::<GameCore>().seed;
        assert_eq!(
            snapshot(&app),
            Game::with_config(run_seed, &modes::mode_config(ModeId::Sprint)).snapshot(),
            "core untouched through the whole pre-roll"
        );

        fixed_step(&mut app);
        assert_eq!(countdown(&app), 0);
        assert_eq!(steps(&app), 1, "core tick 0 is the first playable frame");
        assert!(
            app.world().resource::<PendingActions>().queue.is_empty(),
            "held action applies on the first playable frame"
        );
    }

    #[test]
    fn pause_during_pre_roll_freezes_then_resumes_countdown() {
        let mut app = test_app(0x52);
        start_mode(&mut app, ModeId::Dig);
        for _ in 0..90 {
            fixed_step(&mut app);
        }
        assert_eq!(countdown(&app), 90);

        app.world_mut().resource_mut::<SimPaused>().0 = true;
        for _ in 0..30 {
            fixed_step(&mut app);
        }
        assert_eq!(
            countdown(&app),
            90,
            "pause must freeze, not cancel, pre-roll"
        );
        assert_eq!(steps(&app), 0);

        app.world_mut().resource_mut::<SimPaused>().0 = false;
        for _ in 0..89 {
            fixed_step(&mut app);
        }
        assert_eq!(countdown(&app), 1);
        assert_eq!(steps(&app), 0);
        fixed_step(&mut app);
        assert_eq!(countdown(&app), 0);
        assert_eq!(
            steps(&app),
            1,
            "resumed pre-roll still ends in a playable frame"
        );
    }

    #[test]
    fn ultra_time_up_flips_game_over_and_exposes_terminal_reason() {
        let mut app = test_app(0x53);
        start_mode(&mut app, ModeId::Ultra);
        assert_eq!(countdown(&app), 0, "Ultra has no pre-roll");

        let mut saw_timeup = false;
        for _ in 0..7300 {
            fixed_step(&mut app);
            if drained(&mut app)
                .iter()
                .any(|e| matches!(e.0, GameEvent::TimeUp { .. }))
            {
                saw_timeup = true;
                break;
            }
        }
        assert!(saw_timeup, "CoreEvent(TimeUp) observable on the wire");
        assert_eq!(
            *app.world().resource::<AppState>(),
            AppState::GameOver,
            "TimeUp flips AppState exactly like GameOver"
        );
        let core = app.world().non_send::<GameCore>();
        assert_eq!(core.game.finished_reason(), Some(FinishReason::TimeUp));
        assert_eq!(core.game.tick_count(), 7200);
    }

    #[test]
    fn goal_reached_event_flips_game_over_and_forwards_too() {
        let mut app = test_app(0x57);
        app.world_mut()
            .non_send_mut::<GameCore>()
            .pending_events
            .push(GameEvent::GoalReached { tick: 5 });

        fixed_step(&mut app);

        assert_eq!(*app.world().resource::<AppState>(), AppState::GameOver);
        let events = drained(&mut app);
        assert!(
            events
                .iter()
                .any(|e| matches!(e.0, GameEvent::GoalReached { tick: 5 })),
            "CoreEvent forwarding preserved for terminal events: {events:?}"
        );
    }

    #[test]
    fn sprint_top_out_flips_game_over_without_any_goal_event() {
        let mut app = test_app(0x54);
        start_mode(&mut app, ModeId::Sprint);
        for _ in 0..180 {
            fixed_step(&mut app);
        }

        let mut saw_goal = false;
        let mut saw_game_over = false;
        for _ in 0..1000 {
            app.world_mut()
                .resource_mut::<PendingActions>()
                .push(Action::HardDrop);
            fixed_step(&mut app);
            for event in drained(&mut app) {
                if matches!(event.0, GameEvent::GoalReached { .. }) {
                    saw_goal = true;
                }
                if event.0 == GameEvent::GameOver {
                    saw_game_over = true;
                }
            }
            if saw_game_over {
                break;
            }
        }
        assert!(
            saw_game_over,
            "hard-drop pile-up must block out under Sprint rules"
        );
        assert!(!saw_goal, "top-out must not produce a goal event");
        assert_eq!(*app.world().resource::<AppState>(), AppState::GameOver);
        assert_eq!(
            app.world().non_send::<GameCore>().game.finished_reason(),
            Some(FinishReason::TopOut)
        );
    }

    #[test]
    fn start_mode_bumps_plays_for_the_right_mode_key() {
        let mut app = test_app(0x55);
        app.init_resource::<Records>();
        for id in [
            ModeId::Marathon,
            ModeId::Sprint,
            ModeId::Sprint,
            ModeId::Dig,
        ] {
            start_mode(&mut app, id);
        }
        let records = app.world().resource::<Records>();
        use crate::records::{DIG, MARATHON, SPRINT, ULTRA};
        assert_eq!(records.plays(MARATHON), 1);
        assert_eq!(records.plays(SPRINT), 2);
        assert_eq!(records.plays(DIG), 1);
        assert_eq!(records.plays(ULTRA), 0, "only the started modes count");
    }

    #[test]
    fn restart_on_r_retries_the_current_mode_not_marathon() {
        let mut app = test_app(0x56);
        app.init_resource::<Records>();
        start_mode(&mut app, ModeId::Sprint);
        for _ in 0..180 {
            fixed_step(&mut app);
        }
        *app.world_mut().resource_mut::<AppState>() = AppState::GameOver;

        app.insert_resource(ButtonInput::<KeyCode>::default());
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyR);
        let _ = app.world_mut().try_run_schedule(Update);

        assert_eq!(*app.world().resource::<AppState>(), AppState::Playing);
        assert_eq!(
            app.world().non_send::<GameCore>().active_mode.id,
            ModeId::Sprint,
            "R retries the recorded mode"
        );
        assert_eq!(
            countdown(&app),
            modes::pre_roll_ticks(ModeId::Sprint),
            "retry re-arms the mode's pre-roll"
        );
        assert_eq!(steps(&app), 0);
        assert_eq!(
            app.world()
                .resource::<Records>()
                .plays(crate::records::SPRINT),
            2
        );
    }

    #[test]
    fn bot_solver_prefers_line_clear_over_flat_stack() {
        // Four-cell gap in the settled bottom row, I piece active: the solver
        // must slide the horizontal I into the gap to clear the row rather
        // than rest it flat on top of the stack.
        let mut board = Board::new();
        for col in 0..COLS {
            board.set(ROWS - 1, col, Some(tetris_core::piece::Piece::O));
        }
        for col in 3..7 {
            board.set(ROWS - 1, col, None);
        }
        let snapshot = GameSnapshot {
            board,
            active: Some(PieceState {
                piece: tetris_core::piece::Piece::I,
                rot: Rotation::Spawn,
                row: 0,
                col: 4,
            }),
            ghost_row: None,
            hold: None,
            hold_used: false,
            next: Vec::new(),
            score: 0,
            level: 1,
            lines: 0,
            combo: 0,
            b2b: false,
            game_over: false,
        };
        let mv = bot_move(&snapshot).expect("I always fits somewhere");
        assert_eq!(mv.rot, Rotation::Spawn, "flat I fills the four-gap");
        assert_eq!(mv.target_col, 3, "{mv:?} fills cols 3..=6");
    }

    // ---- T8: ModeHudInfo refresh ----

    fn mode_hud(app: &App) -> ModeHudInfo {
        *app.world().resource::<ModeHudInfo>()
    }

    #[test]
    fn mode_hud_defaults_to_hidden_marathon() {
        let app = test_app(0x70);
        let hud = mode_hud(&app);
        assert_eq!(hud.mode_id, ModeId::Marathon);
        assert!(!hud.show_hud, "Marathon renders no clock/goal row");
        assert_eq!(hud.lines_left, None);
        assert_eq!(hud.garbage_left, None);
        assert_eq!(hud.clock_limit, None);
        assert_eq!(hud.feed_pending, None);
        assert_eq!(hud.feed_next_row_in, None);
        assert_eq!(hud.swap_in, None);
    }

    #[test]
    fn mode_hud_refresh_reads_getters_per_mode() {
        let mut app = test_app(0x71);
        start_mode(&mut app, ModeId::Sprint);
        fixed_step(&mut app);
        let hud = mode_hud(&app);
        assert_eq!(hud.mode_id, ModeId::Sprint);
        assert!(hud.show_hud);
        assert_eq!(hud.lines_left, Some(modes::SPRINT_GOAL_LINES));
        assert_eq!(hud.garbage_left, None);
        assert_eq!(hud.clock_limit, None, "Sprint counts up");
        assert_eq!(hud.countdown, 179);

        start_mode(&mut app, ModeId::Ultra);
        fixed_step(&mut app);
        let hud = mode_hud(&app);
        assert_eq!(hud.clock_limit, Some(modes::ULTRA_CLOCK_TICKS));
        assert_eq!(hud.clock_ticks, 1, "refreshed from game.tick_count()");
        assert_eq!(hud.lines_left, None);

        start_mode(&mut app, ModeId::Dig);
        fixed_step(&mut app);
        let hud = mode_hud(&app);
        assert_eq!(hud.garbage_left, Some(modes::DIG_GARBAGE_ROWS));
        assert_eq!(hud.lines_left, None);
        assert!(hud.show_hud);
    }

    #[test]
    fn mode_hud_counts_piece_locks_and_resets_on_new_run() {
        let mut app = test_app(0x72);
        for _ in 0..3 {
            app.world_mut()
                .resource_mut::<PendingActions>()
                .push(Action::HardDrop);
            fixed_step(&mut app);
        }
        assert_eq!(mode_hud(&app).pieces_placed, 3, "one per PieceLocked event");

        start_mode(&mut app, ModeId::Sprint);
        fixed_step(&mut app);
        assert_eq!(
            mode_hud(&app).pieces_placed,
            0,
            "steps == 0 resets the count"
        );
    }
}
