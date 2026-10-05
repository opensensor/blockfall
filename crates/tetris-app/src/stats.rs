//! Per-run aggregate stats — fed from the [`CoreEvent`] stream and shown on
//! the game-over screen ("PPS 0.62 - 14/min - 3 t-spins - 2 tetrises -
//! combo x5"). Deliberately cheap: counters only, no history; the run clock
//! is `GameCore::steps` (reset to 0 by every `start_mode`/`restart_with`,
//! the house run-start signal already relied on by the HUD piece counter).

use bevy::prelude::*;

use tetris_core::event::GameEvent;

use crate::core_bridge::CoreEvent;
use crate::state::AppState;

/// Aggregate counters for the run in progress (or the one that just ended,
/// frozen while [`AppState::GameOver`] is on screen).
#[derive(Debug, Default, Clone, PartialEq, Eq, Resource)]
pub struct RunStats {
    /// Pieces locked this run.
    pub pieces: u32,
    /// Lines cleared this run.
    pub lines: u32,
    /// Four-line clears.
    pub tetrises: u32,
    /// T-spin locks (mini counts).
    pub tspins: u32,
    /// Highest combo streak reached.
    pub max_combo: u32,
}

/// Reset on run start (`AppState` → `Playing`, the same edge the core uses
/// to zero `steps`) and fold every core event into the counters.
pub fn feed_run_stats(
    mut reader: MessageReader<CoreEvent>,
    mut stats: ResMut<RunStats>,
    state: Res<AppState>,
) {
    if state.is_changed() && *state == AppState::Playing {
        *stats = RunStats::default();
    }
    for CoreEvent(event) in reader.read() {
        match event {
            GameEvent::PieceLocked { .. } => stats.pieces += 1,
            GameEvent::LineCleared { lines } => {
                stats.lines += *lines as u32;
                if *lines == 4 {
                    stats.tetrises += 1;
                }
            }
            GameEvent::TSpinDetected { .. } => stats.tspins += 1,
            GameEvent::ComboChanged { n } => stats.max_combo = stats.max_combo.max(*n),
            _ => {}
        }
    }
}

/// Game-over display line, derived purely from the counters and the run's
/// fixed-step count (60 steps = 1 s). ASCII only (bundled font subset).
#[must_use]
pub fn format_run_stats(stats: &RunStats, steps: u64) -> String {
    let secs = (steps as f64 / 60.0).max(1.0);
    let pps = stats.pieces as f64 / secs;
    let lines_per_min = stats.lines as f64 / secs * 60.0;
    format!(
        "PPS {pps:.2} - {:.0}/min - {} t-spins - {} tetrises - combo x{}",
        lines_per_min, stats.tspins, stats.tetrises, stats.max_combo
    )
}

/// Owns [`RunStats`]; mount next to the core bridge. The stats *display*
/// lives in `screens_menu` and reads this resource optionally, so headless
/// plugin stubs without it stay green.
pub struct RunStatsPlugin;

impl Plugin for RunStatsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RunStats>()
            .add_message::<CoreEvent>()
            .add_systems(Update, feed_run_stats);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tetris_core::piece::{Piece, PieceState, Rotation};

    fn locked(events: &mut bevy::ecs::message::Messages<CoreEvent>) {
        events.write(CoreEvent(GameEvent::PieceLocked {
            piece: Piece::T,
            state: PieceState {
                piece: Piece::T,
                rot: Rotation::default(),
                row: 2,
                col: 4,
            },
        }));
    }

    #[test]
    fn counters_fold_and_reset_on_run_start() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<CoreEvent>()
            .init_resource::<AppState>()
            .init_resource::<RunStats>()
            .add_systems(Update, feed_run_stats);
        {
            let mut events = app.world_mut().resource_mut::<Messages<CoreEvent>>();
            locked(&mut events);
            locked(&mut events);
            events.write(CoreEvent(GameEvent::LineCleared { lines: 4 }));
            events.write(CoreEvent(GameEvent::TSpinDetected {
                kind: tetris_core::tspin::TSpinKind::Full,
            }));
            events.write(CoreEvent(GameEvent::ComboChanged { n: 3 }));
            events.write(CoreEvent(GameEvent::ComboChanged { n: 1 }));
        }
        app.update();
        {
            let stats = app.world().resource::<RunStats>();
            assert_eq!(stats.pieces, 2);
            assert_eq!(stats.lines, 4);
            assert_eq!(stats.tetrises, 1);
            assert_eq!(stats.tspins, 1);
            assert_eq!(stats.max_combo, 3);
        }
        // A fresh run resets every counter.
        *app.world_mut().resource_mut::<AppState>() = AppState::GameOver;
        app.update();
        *app.world_mut().resource_mut::<AppState>() = AppState::Playing;
        app.update();
        assert_eq!(*app.world().resource::<RunStats>(), RunStats::default());
    }

    #[test]
    fn format_is_ascii_and_clock_based() {
        let stats = RunStats {
            pieces: 62,
            lines: 20,
            tetrises: 2,
            tspins: 3,
            max_combo: 5,
        };
        // A 100 s run (6 000 fixed steps): 0.62 pieces/s, 12 lines/min.
        let line = format_run_stats(&stats, 6_000);
        assert!(line.is_ascii());
        assert_eq!(
            line,
            "PPS 0.62 - 12/min - 3 t-spins - 2 tetrises - combo x5"
        );
        // Zero/near-zero clock never divides: clamps at one second.
        assert!(format_run_stats(&stats, 0).starts_with("PPS 62.00"));
    }
}
