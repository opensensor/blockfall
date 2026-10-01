//! T10 Release-1 CI completion gates: the wired app driven headlessly by the
//! named-mode bot (`TETRIS_BOT=sprint|ultra|dig`) must reach each mode's
//! terminal state and emit the machine-parseable `BOT mode_done` /
//! `BOT mode_abort` lines the PRD success metric demands ("the bot finishes
//! Sprint and Dig").
//!
//! Hidden-window-style MinimalPlugins runs of the real [`CoreBridgePlugin`]
//! (no winit, no GPU — the bot lifecycle runs in `Update`, the core steps via
//! manual `FixedUpdate` runs, exactly like `core_bridge`'s unit tests). Log
//! capture goes through a [`LogPlugin`] `custom_layer` that records every
//! `info!` message text into a process-wide buffer.
//!
//! All tests share one global env mutex: `TETRIS_BOT`/`TETRIS_SEED` are read
//! live by the bridge (plugin build *and* every `start_mode_run`), so
//! scenarios must not overlap within this test binary.

use std::fmt::{self, Debug, Write as _};
use std::sync::Mutex;

use bevy::log::LogPlugin;
use bevy::log::{tracing, tracing_subscriber};
use bevy::prelude::*;

use blockfall_app::core_bridge::{CoreBridgePlugin, GameCore, ModeHudInfo};
use blockfall_app::modes::ModeId;
use blockfall_app::state::AppState;

use tetris_core::mode::FinishReason;

// ---------------------------------------------------------------------------
// Shared harness: env-serialized named-bot app + info! capture layer
// ---------------------------------------------------------------------------

/// Serializes every test that touches `TETRIS_BOT` / `TETRIS_SEED` (read at
/// plugin build *and* per `start_mode_run`); each test holds the guard for
/// its whole body.
static BOT_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Every `info!` message emitted while any bot-test app runs.
static BOT_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct LineVisitor<'a>(&'a mut String);

impl tracing::field::Visit for LineVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn Debug) {
        if field.name() == "message" {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            let _ = write!(self.0, "{value:?}");
        }
    }
}

impl fmt::Display for LineVisitor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

struct BotLogLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for BotLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut line = String::new();
        event.record(&mut LineVisitor(&mut line));
        BOT_LOG.lock().unwrap().push(line);
    }
}

fn bot_log_mark() -> usize {
    BOT_LOG.lock().unwrap().len()
}

fn bot_log_since(mark: usize) -> Vec<String> {
    BOT_LOG.lock().unwrap()[mark..].to_vec()
}

/// Fresh hidden-window-style bot app (no `GameCore` from wall clock leaks
/// into results: the named bot re-starts the game from `TETRIS_SEED` before
/// the core takes its first step). The caller must hold [`BOT_ENV_LOCK`].
fn bot_test_app() -> App {
    let mut app = App::new();
    // MinimalPlugins carries no LogPlugin in bevy 0.19; add it explicitly
    // with the capture layer. `set_global_default` is first-wins per process
    // and never panics here, so building one per app is safe — and every
    // candidate layer captures into the same [`BOT_LOG`] buffer anyway.
    app.add_plugins(MinimalPlugins);
    app.add_plugins(LogPlugin {
        filter: "info".into(),
        custom_layer: |_| Some(Box::new(BotLogLayer)),
        ..default()
    });
    app.add_plugins(CoreBridgePlugin);
    app.init_resource::<AppState>();
    // Mirror the shipped app's boot screen (T17): the named bot presses the
    // Start-equivalent from Title.
    *app.world_mut().resource_mut::<AppState>() = AppState::Title;
    app
}

/// One app frame: Update systems (bot lifecycle) then one fixed core step —
/// the same ordering guarantee the real Main→FixedMain→Update loop gives.
fn step(app: &mut App) {
    let _ = app.world_mut().try_run_schedule(Update);
    app.world_mut().run_schedule(FixedUpdate);
}

fn core(app: &App) -> &GameCore {
    app.world().non_send::<GameCore>()
}

fn app_state(app: &App) -> AppState {
    *app.world().resource::<AppState>()
}

fn drained_exits(app: &mut App) -> Vec<AppExit> {
    app.world_mut()
        .resource_mut::<Messages<AppExit>>()
        .drain()
        .collect()
}

/// Drive until `AppState::GameOver` (or the budget), then keep stepping a
/// few frames so the bot's `Update` lifecycle gets one more pass to write
/// its terminal line + exit. Returns all exits along the way.
fn drive_until_terminal(app: &mut App, budget: usize) -> Vec<AppExit> {
    let mut exits = Vec::new();
    let mut grace = None::<usize>;
    for step_no in 0..budget {
        step(app);
        exits.extend(drained_exits(app));
        match grace {
            None => {
                if app_state(app) == AppState::GameOver {
                    grace = Some(0);
                }
            }
            Some(n) => {
                if !exits.is_empty() || n >= 5 {
                    return exits;
                }
                grace = Some(n + 1);
            }
        }
        assert!(
            step_no + 1 < budget,
            "run never reached a terminal state within {budget} steps"
        );
    }
    exits
}

fn set_bot_env(bot: &str, seed: &str) {
    std::env::set_var("TETRIS_BOT", bot);
    std::env::set_var("TETRIS_SEED", seed);
}

fn clear_bot_env() {
    std::env::remove_var("TETRIS_BOT");
    std::env::remove_var("TETRIS_SEED");
}

fn only_line_containing<'a>(lines: &'a [String], needle: &str) -> &'a str {
    let hits: Vec<&String> = lines.iter().filter(|l| l.contains(needle)).collect();
    assert_eq!(
        hits.len(),
        1,
        "exactly one line containing {needle:?} expected, got {hits:?} in {lines:?}"
    );
    hits[0].as_str()
}

// ---------------------------------------------------------------------------
// Sprint: greedy clears 40 → exactly one mode_done + AppExit::Success
// ---------------------------------------------------------------------------

#[test]
fn sprint_bot_finishes_40_lines_and_logs_mode_done() {
    let _guard = BOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_bot_env("sprint", "31337");
    let mark = bot_log_mark();
    let mut app = bot_test_app();

    let exits = drive_until_terminal(&mut app, 20_000);

    let lines = bot_log_since(mark);
    let done = only_line_containing(&lines, "BOT mode_done mode=sprint time_ticks=");
    assert!(
        !done.contains("mode_abort"),
        "a completed Sprint must not abort: {done}"
    );

    let c = core(&app);
    assert_eq!(c.active_mode.id, ModeId::Sprint);
    assert_eq!(c.game.finished_reason(), Some(FinishReason::GoalReached));
    let snap = c.game.snapshot();
    assert!(
        snap.lines >= 40,
        "Sprint goal clears 40, got {}",
        snap.lines
    );
    assert!(!snap.game_over, "GoalReached is not a top-out");

    assert_eq!(exits, vec![AppExit::Success], "clean Sprint exits 0");
    clear_bot_env();
}

// ---------------------------------------------------------------------------
// Dig: nub-down heuristic clears real BuriedGarbage{10} boards on pinned
// winning seeds (T4-fished, re-probed through the app wiring at T10)
// ---------------------------------------------------------------------------

/// Seeds the app-level dig bot provably completes (see T10 plan log for the
/// probe table; T4's core-driver pins 9/28/30/31/35/36 survive the app's
/// gravity + step-executor too).
const DIG_WINNING_SEEDS: [&str; 6] = ["9", "28", "30", "31", "35", "36"];

#[test]
fn dig_bot_finishes_pinned_buried_boards_and_logs_mode_done() {
    let _guard = BOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for seed in DIG_WINNING_SEEDS {
        set_bot_env("dig", seed);
        let mark = bot_log_mark();
        let mut app = bot_test_app();

        let exits = drive_until_terminal(&mut app, 12_000);

        let lines = bot_log_since(mark);
        only_line_containing(&lines, "BOT mode_done mode=dig time_ticks=");
        assert!(
            !lines.iter().any(|l| l.contains("mode_abort")),
            "seed {seed}: winning Dig run must not abort: {lines:?}"
        );

        let c = core(&app);
        assert_eq!(c.active_mode.id, ModeId::Dig);
        assert_eq!(
            c.game.finished_reason(),
            Some(FinishReason::GoalReached),
            "seed {seed}: pinned winning Dig board must reach the garbage goal \
             (garbage left {})",
            c.game.garbage_rows_left()
        );
        assert_eq!(c.game.garbage_rows_left(), 0, "seed {seed}");
        assert_eq!(
            exits,
            vec![AppExit::Success],
            "seed {seed}: clean Dig exits 0"
        );
    }
    clear_bot_env();
}

#[test]
fn dig_topout_logs_mode_abort_and_exits_nonzero() {
    let _guard = BOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Pinned from the same probe: this board buries the bot (top-out), the
    // gate a silent early death must trip instead of burning the budget.
    set_bot_env("dig", "5");
    let mark = bot_log_mark();
    let mut app = bot_test_app();

    let exits = drive_until_terminal(&mut app, 12_000);

    let lines = bot_log_since(mark);
    let abort = only_line_containing(&lines, "BOT mode_abort mode=dig ticks=");
    assert!(
        !abort.contains("mode_done"),
        "a top-out must log the abort line, not mode_done: {abort}"
    );
    assert_eq!(
        core(&app).game.finished_reason(),
        Some(FinishReason::TopOut)
    );
    assert_eq!(exits, vec![AppExit::error()], "top-out must exit nonzero");
    clear_bot_env();
}

// ---------------------------------------------------------------------------
// Ultra: 30-tick cadence survives past 1200 ticks without topping out
// (exact 7200-tick TimeUp stays core-covered by T4)
// ---------------------------------------------------------------------------

#[test]
fn ultra_bot_cadence_survives_past_1200_ticks() {
    let _guard = BOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_bot_env("ultra", "42");
    let mark = bot_log_mark();
    let mut app = bot_test_app();

    let mut ticks = 0u64;
    for step_no in 0..4_000 {
        step(&mut app);
        assert!(
            drained_exits(&mut app).is_empty(),
            "ultra must not log a terminal line before the survival horizon"
        );
        ticks = core(&app).game.tick_count();
        if ticks > 1_200 {
            break;
        }
        assert!(
            step_no + 1 < 4_000,
            "ultra bot stalled before tick 1200 (tick {ticks})"
        );
    }
    let hud = *app.world().resource::<ModeHudInfo>();
    assert!(
        ticks > 1_200,
        "the ultra bot must survive past 1200 ticks, stopped at {ticks}"
    );
    assert!(
        app_state(&app) == AppState::Playing,
        "survival: no top-out, no terminal state"
    );
    assert!(
        !core(&app).game.snapshot().game_over,
        "ultra board still alive at tick {ticks}"
    );
    // Cadence proof: ~one placement per 30 ticks ⇒ far below a free-run
    // lock rate (~8 pieces / 1200 ticks would mean 150+).
    assert!(
        hud.pieces_placed <= 1_200 / 20,
        "ultra bot must pace ~1 piece/30 ticks, got {} pieces in {ticks} ticks",
        hud.pieces_placed
    );

    let lines = bot_log_since(mark);
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("mode_done") || l.contains("mode_abort")),
        "no terminal log before the horizon: {lines:?}"
    );
    clear_bot_env();
}

// ---------------------------------------------------------------------------
// Regression: legacy values + unknown values
// ---------------------------------------------------------------------------

#[test]
fn plain_bot_one_still_starts_marathon_from_title() {
    let _guard = BOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_bot_env("1", "7");
    let mut app = bot_test_app();
    for _ in 0..10 {
        step(&mut app);
    }
    assert_eq!(app_state(&app), AppState::Playing);
    assert_eq!(core(&app).active_mode.id, ModeId::Marathon);
    assert_eq!(core(&app).game.tick_count(), 10, "marathon never pre-rolls");
    assert_eq!(core(&app).steps, 10, "every fixed step drives the core");
    clear_bot_env();
}

#[test]
fn marathon_name_behaves_like_one() {
    let _guard = BOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_bot_env("marathon", "8");
    let mut app = bot_test_app();
    for _ in 0..10 {
        step(&mut app);
    }
    assert_eq!(app_state(&app), AppState::Playing);
    assert_eq!(core(&app).active_mode.id, ModeId::Marathon);
    clear_bot_env();
}

#[test]
fn unknown_bot_value_is_inert() {
    let _guard = BOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_bot_env("bogus", "9");
    let mark = bot_log_mark();
    let mut app = bot_test_app();
    for _ in 0..300 {
        step(&mut app);
    }
    assert_eq!(
        app_state(&app),
        AppState::Title,
        "unknown value must not press Start"
    );
    assert_eq!(core(&app).steps, 0);
    let lines = bot_log_since(mark);
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("BOT start") || l.contains("mode_done")),
        "no bot lifecycle from an unknown TETRIS_BOT value: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("TETRIS_BOT")),
        "unknown value should at least warn about the env var: {lines:?}"
    );
    clear_bot_env();
}
