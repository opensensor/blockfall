//! Input map, key bindings, and DAS/ARR state machine (T12).

use bevy::app::{App, Plugin};

/// Translates bound inputs into core `Action`s with DAS/ARR repeat handling.
pub struct InputPlugin;

impl Plugin for InputPlugin {
    fn build(&self, _app: &mut App) {}
}
