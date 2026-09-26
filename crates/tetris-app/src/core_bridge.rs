//! Fixed-timestep bridge between Bevy and `tetris-core::Game` (T10).

use bevy::app::{App, Plugin};

/// Steps the deterministic core on the fixed-step schedule and drains its
/// events into Bevy `Events<GameEvent>`.
pub struct CoreBridgePlugin;

impl Plugin for CoreBridgePlugin {
    fn build(&self, _app: &mut App) {}
}
