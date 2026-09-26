//! Audio: SFX, looping BGM, pause ducking (T18).

use bevy::app::{App, Plugin};

/// Maps core `GameEvent`s to SFX and manages BGM with ducking.
pub struct AudioPlugin;

impl Plugin for AudioPlugin {
    fn build(&self, _app: &mut App) {}
}
