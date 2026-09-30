//! App-side shared state types — **single owner is this file (T1)**.
//!
//! Later tasks consume these types (`T10`–`T19`) and must not reshape them.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Effects/juice quality tier (PRD §8: "all juice skippable" → Low).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectsQuality {
    /// No shake, no freeze frames, static ghost.
    Low,
    /// Balanced default tier.
    #[default]
    Medium,
    /// Full juice budget.
    High,
}

/// User-configurable settings; persisted as JSON by `settings_persist` (T15)
/// and edited by the settings screen (T16). Defaults are PRD §6.5/§6.3/§9.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Resource)]
pub struct Settings {
    /// Delayed Auto Shift before horizontal autorepeat starts, ms (PRD §6.5).
    pub das_ms: u32,
    /// Autorepeat rate between shifted cells, ms (PRD §6.5).
    pub arr_ms: u32,
    /// Master volume in `0.0..=1.0`.
    pub master_volume: f32,
    /// SFX volume in `0.0..=1.0`.
    pub sfx_volume: f32,
    /// BGM volume in `0.0..=1.0`.
    pub music_volume: f32,
    /// Next-queue preview size, clamped by consumers to `1..=6` (PRD §6.3).
    pub next_queue_size: u8,
    /// Soft-drop gravity speed multiplier (PRD §6.5).
    pub soft_drop_multiplier: u8,
    /// Juice/effects quality tier.
    pub effects: EffectsQuality,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            das_ms: 150,
            arr_ms: 33,
            master_volume: 1.0,
            sfx_volume: 0.8,
            music_volume: 0.6,
            next_queue_size: 5,
            soft_drop_multiplier: 20,
            effects: EffectsQuality::default(),
        }
    }
}

/// High-level application state machine (PRD §7 screens).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Resource)]
pub enum AppState {
    /// The core loop is stepping; gameplay inputs flow.
    #[default]
    Playing,
    /// Title screen (T17).
    Title,
    /// Paused overlay; simulation frozen (T17).
    Paused,
    /// Settings screen (T16).
    Settings,
    /// Game-over screen with final score/level/lines + best (T17).
    GameOver,
}

/// Resource marking that a key binding is currently being captured. It is
/// always present (init'd in `main.rs`); `capturing` is set/cleared by the
/// settings screen (T16) and `input` (T12) must suppress core `Action`
/// emission while it is `true`. The specific slot under capture is owned by
/// T16's own plugin resources.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Resource)]
pub struct RebindingCapture {
    /// `true` while the settings screen is waiting for a key press.
    pub capturing: bool,
}

/// Cross-plugin schedule anchor: the pause-chord consumer must evaluate the
/// capture flag before the settings screen clears a capture that went stale
/// outside Settings. Both plugins configure the set, so either plugin may be
/// used standalone in headless tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SystemSet)]
pub enum CaptureOrder {
    /// Pause-chord consumer (`screens_menu`); observes a pre-cleanup
    /// [`RebindingCapture`].
    Chord,
    /// Capture cleanup (`screens_settings`); runs after [`Self::Chord`].
    Cleanup,
}

/// Headless smoke tests for the T1 scaffold. They live in this file because it
/// is the one app module permanently owned by T1; later tasks must not delete
/// them.
#[cfg(test)]
mod smoke_tests {
    use super::{AppState, RebindingCapture, Settings};
    use crate::{
        audio::AudioPlugin, core_bridge::CoreBridgePlugin, hud::HudPlugin, input::InputPlugin,
        juice::JuicePlugin, render::RenderPlugin, screens_menu::MenuScreensPlugin,
        screens_settings::SettingsScreenPlugin, settings_persist::SettingsPersistPlugin,
    };
    use bevy::{
        prelude::*,
        window::{Window, WindowPlugin},
    };

    const SMOKE_FRAMES: u32 = 10;

    fn add_app_plugins(app: &mut App) {
        app.add_plugins((
            CoreBridgePlugin,
            SettingsPersistPlugin,
            RenderPlugin,
            InputPlugin,
            HudPlugin,
            MenuScreensPlugin,
            SettingsScreenPlugin,
            AudioPlugin,
            JuicePlugin,
        ));
    }

    fn insert_shared_state(app: &mut App) {
        app.init_resource::<Settings>()
            .init_resource::<AppState>()
            .init_resource::<RebindingCapture>();
    }

    /// Pins `TETRIS_CONFIG_DIR` to a fresh empty directory for the test's
    /// duration, holding the shared `settings_persist` env lock so a
    /// parallel env-mutating test cannot point the plugin's startup load at
    /// *another* test's temp dir (the historical `das_ms 111 vs 150` flake).
    struct ConfigDirGuard {
        _env: std::sync::MutexGuard<'static, ()>,
        dir: std::path::PathBuf,
    }

    impl Drop for ConfigDirGuard {
        fn drop(&mut self) {
            std::env::remove_var(crate::settings_persist::CONFIG_DIR_ENV);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn isolate_config_dir(label: &str) -> ConfigDirGuard {
        let _env = crate::settings_persist::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "blockfall-smoke-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("smoke temp dir");
        std::env::set_var(crate::settings_persist::CONFIG_DIR_ENV, &dir);
        ConfigDirGuard { _env, dir }
    }

    #[test]
    fn app_with_all_plugin_stubs_runs_frames_without_a_window() {
        let _config = isolate_config_dir("frames");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        add_app_plugins(&mut app);
        insert_shared_state(&mut app);
        for _ in 0..SMOKE_FRAMES {
            app.update();
        }
        assert_eq!(*app.world().resource::<AppState>(), AppState::Playing);
        assert_eq!(app.world().resource::<Settings>().das_ms, 150);
        assert!(!app.world().resource::<RebindingCapture>().capturing);
    }

    /// True-headless hidden-window smoke test: intentionally **no** winit
    /// (winit refuses event-loop creation off the main thread, which every
    /// cargo-test thread is). `WindowPlugin` alone spawns the primary
    /// `Window` entity, so the full plugin stub tree runs against a
    /// `visible: false` window with no display server involved.
    #[test]
    fn app_with_hidden_window_builds_runs_and_exits_after_n_frames() {
        let _config = isolate_config_dir("hidden");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris headless smoke test".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        add_app_plugins(&mut app);
        insert_shared_state(&mut app);
        for _ in 0..SMOKE_FRAMES {
            app.update();
        }

        let mut query = app.world_mut().query::<&Window>();
        let windows: Vec<&Window> = query.iter(app.world()).collect();
        assert_eq!(windows.len(), 1, "exactly one primary window expected");
        assert!(!windows[0].visible, "smoke window must stay hidden");
        assert_eq!(windows[0].resolution.physical_width(), 1280);
        assert_eq!(*app.world().resource::<AppState>(), AppState::Playing);
    }
}
