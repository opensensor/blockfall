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
use tetris_core::event::GameEvent;
use tetris_core::game::Game;

use crate::state::AppState;

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
}

impl GameCore {
    /// Fresh run from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            game: Game::new(seed),
            seed,
            steps: 0,
            pending_events: Vec::new(),
        }
    }

    /// Start a brand-new run seeded from wall time (T17's Game Over → retry).
    pub fn restart(&mut self) {
        let seed = wall_clock_seed();
        self.restart_with(seed);
    }

    /// Deterministic variant of [`GameCore::restart`] for tests and replays.
    pub fn restart_with(&mut self, seed: u64) {
        self.game = Game::new(seed);
        self.seed = seed;
        self.steps = 0;
        self.pending_events.clear();
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

/// Steps the core on the fixed schedule. Registered in `FixedUpdate` (a
/// `FixedMain` sub-schedule run ahead of `Update`/render — never in a render
/// or `Update` schedule).
fn core_bridge_system(
    core: NonSendMut<GameCore>,
    mut pending: ResMut<PendingActions>,
    mut messages: MessageWriter<CoreEvent>,
    mut app_state: ResMut<AppState>,
    paused: Res<SimPaused>,
) {
    let core = core.into_inner();
    if !paused.0 && *app_state == AppState::Playing {
        for action in pending.queue.drain(..) {
            core.pending_events.extend(core.game.apply(action));
        }
        core.pending_events.extend(core.game.tick());
        core.steps += 1;
    }

    let mut game_over = false;
    for event in core.pending_events.drain(..) {
        if event == GameEvent::GameOver {
            game_over = true;
        }
        messages.write(CoreEvent(event));
    }
    if game_over && *app_state == AppState::Playing {
        *app_state = AppState::GameOver;
    }
}

/// Spawns the primary 2D camera if no other plugin stub has one yet (render
/// logic itself is T11's; this keeps the shipped app displayable).
fn spawn_primary_camera(mut commands: Commands, cameras: Query<&Camera2d>) {
    if cameras.is_empty() {
        commands.spawn(Camera2d);
    }
}

/// Steps the deterministic core on the fixed-step schedule and drains its
/// events into Bevy `Messages<CoreEvent>`.
pub struct CoreBridgePlugin;

impl Plugin for CoreBridgePlugin {
    fn build(&self, app: &mut App) {
        app.insert_non_send(GameCore::default())
            .init_resource::<PendingActions>()
            .init_resource::<SimPaused>()
            .add_message::<CoreEvent>()
            // Overwrite TimePlugin's default 64 Hz clock: the core contract
            // is 60 Hz (gravity, lock delay, DAS/ARR tick conversions all
            // assume it).
            .insert_resource(Time::<Fixed>::from_hz(SIM_HZ))
            .add_systems(Startup, spawn_primary_camera)
            .add_systems(FixedUpdate, core_bridge_system);
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
}
