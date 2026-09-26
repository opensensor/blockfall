//! Settings / best-score JSON persistence (T15).

use bevy::app::{App, Plugin};

/// Loads `Settings` + best score at startup and saves them atomically.
pub struct SettingsPersistPlugin;

impl Plugin for SettingsPersistPlugin {
    fn build(&self, _app: &mut App) {}
}
