//! `tetris-app` binary entry point.
//!
//! Wired once at T1 per tetris-plan.md: every app module registers its `Plugin`
//! stub here and later tasks fill only their own `Plugin::build()` — this file
//! is never re-edited.

#![allow(dead_code)] // stub items are consumed by later tasks (T10–T19)

mod audio;
mod core_bridge;
mod hud;
mod input;
mod juice;
mod render;
mod screens_menu;
mod screens_settings;
mod settings_persist;
mod state;

use bevy::{
    prelude::*,
    window::{Window, WindowPlugin},
};

use audio::AudioPlugin;
use core_bridge::CoreBridgePlugin;
use hud::HudPlugin;
use input::InputPlugin;
use juice::JuicePlugin;
use render::RenderPlugin;
use screens_menu::MenuScreensPlugin;
use screens_settings::SettingsScreenPlugin;
use settings_persist::SettingsPersistPlugin;
use state::{AppState, RebindingCapture, Settings};

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                ..default()
            }),
            ..default()
        }))
        .init_resource::<Settings>()
        .init_resource::<AppState>()
        .init_resource::<RebindingCapture>()
        .add_plugins((
            CoreBridgePlugin,
            SettingsPersistPlugin,
            RenderPlugin,
            InputPlugin,
            HudPlugin,
            MenuScreensPlugin,
            SettingsScreenPlugin,
            AudioPlugin,
            JuicePlugin,
        ))
        .run();
}
