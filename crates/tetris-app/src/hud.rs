//! Score/level/lines HUD, next-queue preview, and hold box (T13).

use bevy::app::{App, Plugin};

/// Renders the in-game HUD from `Game::snapshot()` and core events.
pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, _app: &mut App) {}
}
