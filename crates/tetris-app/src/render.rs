//! Playfield renderer (T11).

use bevy::app::{App, Plugin};

/// Draws board, active piece, and ghost exclusively from `Game::snapshot()`.
pub struct RenderPlugin;

impl Plugin for RenderPlugin {
    fn build(&self, _app: &mut App) {}
}
